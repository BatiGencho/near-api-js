use std::collections::{HashSet, HashMap};
use futures::sync::mpsc;
use futures::{Future, Poll, Async, Stream, stream, Sink};
use std::borrow::{BorrowMut, Borrow};

use primitives::types::{UID, Gossip, GossipBody, SignedMessageData, StructHash};
use primitives::traits::{Payload, WitnessSelector};
use dag::DAG;

static UNINITIALIZED_DAG_ERR: &'static str = "The DAG structure was not initialized yet.";
static CANDIDATES_OUT_OF_SYNC_ERR: &'static str = "The structures that are used for candidates tracking are ouf ot sync.";

/// A future that owns TxFlow DAG and encapsulates gossiping logic. Should be run as a separate
/// task by a reactor. Consumes a stream of gossips and payloads, and produces a stream of gossips
/// and consensuses. Currently produces only stream of gossips, TODO stream of consensuses.
pub struct TxFlowTask<'a, P: Payload, W: WitnessSelector> {
    owner_uid: UID,
    starting_epoch: u64,
    messages_receiver: mpsc::Receiver<Gossip<P>>,
    payload_receiver: mpsc::Receiver<P>,
    messages_sender: mpsc::Sender<Gossip<P>>,
    witness_selector: Box<W>,
    dag: Option<Box<DAG<'a, P, W>>>,

    /// Buffer for the incoming payloads.
    payload_buffer: Vec<P>,
    /// Received message for which some parents are not in the DAG yet
    /// -> hashes of the parents that are not in the DAG yet.
    candidates: HashMap<SignedMessageData<P>, HashSet<StructHash>>,
    /// The transpose of `candidates`.
    /// Hash of the parent message that we are missing
    /// -> hashes of the messages that depend on it.
    missing_messages: HashMap<StructHash, HashSet<StructHash>>,
    /// Some received messages require a reply to whoever send them to us. This
    /// structure stores this knowledge, so once this message ends up in the DAG
    /// we can send a reply to the sender.
    required_replies: HashMap<StructHash, HashSet<UID>>,

    // TODO: Add mockable timers.
}

impl<'a, P: Payload, W: WitnessSelector> TxFlowTask<'a, P, W> {
    pub fn new(owner_uid: UID,
               starting_epoch: u64,
               messages_receiver: mpsc::Receiver<Gossip<P>>,
               payload_receiver: mpsc::Receiver<P>,
               messages_sender: mpsc::Sender<Gossip<P>>,
               witness_selector: W) -> Self {
        Self {
            owner_uid,
            starting_epoch,
            messages_receiver,
            payload_receiver,
            messages_sender,
            witness_selector: Box::new(witness_selector),
            dag: None,
            payload_buffer: vec![],
            candidates: HashMap::new(),
            missing_messages: HashMap::new(),
            required_replies: HashMap::new(),
        }
    }

    /// Drop the current TxFlow DAG.
    pub fn drop_dag(&mut self) {
        self.dag.take();
    }

    /// Mutable reference to the DAG.
    fn dag_as_mut(&mut self) -> &mut DAG<'a, P, W>{
        self.dag.as_mut().expect(UNINITIALIZED_DAG_ERR).borrow_mut()
    }

    /// Immutable reference to the DAG.
    fn dag_as_ref(&self) -> &DAG<'a, P, W>{
        self.dag.as_ref().expect(UNINITIALIZED_DAG_ERR).borrow()
    }

    /// Sends a gossip by spawning a separate task.
    fn send_gossip(&self, gossip: Gossip<P>) {
        let copied_tx = self.messages_sender.clone();
        tokio::spawn(copied_tx.send(gossip).map(|_| ()).map_err(|e| {
            error!("Failed to send a gossip {:?}", e)
        }));
    }

    /// Process the candidate that now has all necessary parent messages. Add it to the dag
    /// and check whether it makes other candidates passing.
    fn process_passing_candidate(&mut self, message: SignedMessageData<P>) -> HashSet<UID> {
        let hash = message.hash;
        {
            if let Err(e) = self.dag_as_mut().add_existing_message(message) {
                panic!("Attempted to add invalid message to the DAG")
            }
        }

        // Check if there are other candidates that depend on this candidate.
        let mut newly_passing_dependents = vec![];
        if let Some(dependents) = self.missing_messages.remove(&hash) {
            for d in dependents {
                if self.candidates.get(&d).expect(CANDIDATES_OUT_OF_SYNC_ERR).len() == 1 {
                    // This candidate is now able to pass.
                    newly_passing_dependents.push(d);
                }
            }
        }
        let mut new_replies = HashSet::new();
        for d in &newly_passing_dependents {
            let (passing_candidate, _) = self.candidates.remove_entry(d).expect(CANDIDATES_OUT_OF_SYNC_ERR);
            if let Some(replies) = self.required_replies.remove(d) {
                new_replies.extend(replies);
            }
            self.process_passing_candidate(passing_candidate);
        }
        new_replies
    }

    /// Processes the incoming candidate. Returns:
    /// * a set of peers that need a reply;
    /// * a set of hashes that should be fetched if this candidate is missing;
    /// Even if the message itself was not requesting a reply it might have enabled some messages
    /// in `candidates` to be added to the `dag` and these other messages might require a reply.
    fn process_incoming_candidate(&mut self, message: SignedMessageData<P>, reply_to: Option<UID>)
        -> (HashSet<UID>, HashSet<StructHash>) {
        // Check one of the optimistic scenarios when we already know this message, but we still
        // reply on the request.
        if self.dag_as_ref().contains_message(&message.hash) {
            if let Some(uid) = reply_to {
                (set!{uid}, HashSet::new()) } else {
                (HashSet::new(), HashSet::new()) }
        } else if self.candidates.contains_key(&message) {
            // This message is already in the candidates, but we might still need to update required
            // replies.
            if let Some(uid) = reply_to {
                self.required_replies.entry(message.hash).or_insert_with(|| HashSet::new())
                    .insert(uid);
            }
            // No replies needed to be send right now.
            (HashSet::new(), HashSet::new())
        } else {
            let mut unknown_hashes: HashSet<StructHash> = (&message.body.parents).into_iter().filter_map(
                |h| if self.dag_as_ref().contains_message(h) {
                        None } else {
                        Some(*h)
                    } ).collect();
            if unknown_hashes.is_empty() {
                // This candidate is passing. It might have made other candidates passing and these
                // candidates were requesting their replies.
                let mut result = self.process_passing_candidate(message);
                if let Some(uid) = reply_to {
                    result.insert(uid);
                }
                (result, HashSet::new())
            } else {
                // This candidates is not passing, and since it cannot be added to the DAG yet,
                // there are no replies needed. Update the tracking containers.
                for p in &unknown_hashes {
                    self.missing_messages.entry(*p).or_insert_with(|| HashSet::new())
                        .insert(message.hash);
                }
                if let Some(uid) = reply_to {
                    self.required_replies.entry(message.hash).or_insert_with(|| HashSet::new())
                        .insert(uid);
                }
                self.candidates.insert(message, unknown_hashes.drain().collect());
                (HashSet::new(), unknown_hashes)
            }
        }
    }

    /// Take care of the gossip received from the network.
    /// Returns:
    /// * set of the UIDs to which we should send a reply;
    /// * set of hashes that can be fetched from the given message sender.
    fn process_gossip(&mut self, mut gossip: Gossip<P>) -> (HashSet<UID>, HashSet<StructHash>) {
        match gossip.body {
            GossipBody::Unsolicited(message) => self.process_incoming_candidate(message, Some(gossip.sender_uid)),
            GossipBody::UnsolicitedReply(message) => self.process_incoming_candidate(message, None),
            GossipBody::Fetch(ref mut hashes) => {
                let reply_messages: Vec<_> =
                hashes.into_iter().filter_map(
                    |h| self.dag_as_ref()
                        .copy_message_data_by_hash(h)).collect();
                let reply = Gossip {
                    sender_uid: self.owner_uid,
                    receiver_uid: gossip.sender_uid,
                    sender_sig: 0,  // TODO: Sign it.
                    body: GossipBody::FetchReply(reply_messages)
                };
                self.send_gossip(reply);
                // This gossip only requires for an information and does not modify the dag
                // therefore now candidates are getting added to the dag that in turn might require
                // a reply.
                (HashSet::new(), HashSet::new())
            },
            GossipBody::FetchReply(ref mut messages) => {
                // Return the union of all peers that should receive a reply and the union of all
                // hashes that should be now requested.
                let (mut all_uids, mut all_hashes) = (HashSet::new(), HashSet::new());
                messages.drain(..).map(|m|
                    {
                        let (mut new_uids, mut new_hashes) = self.process_incoming_candidate(m, None);
                        all_uids.extend(new_uids.drain());
                        all_hashes.extend(new_hashes.drain());
                    }
                );
                (all_uids, all_hashes)
            },
        }
    }
}

// TxFlowTask can be used as a stream, where each element produced by the stream corresponds to
// an individual step of the algorithm.
impl<'a, P: Payload, W: WitnessSelector> Stream for TxFlowTask<'a, P, W> {
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Process new gossips.
        let mut end_of_gossips = false;
        // Set of UIDs to which we need to reply.
        let mut replies = HashSet::new();
        // UID from which we should request a fetch -> hashes that should be fetched.
        let mut fetch_requests = HashMap::new();
        loop {
            match self.messages_receiver.poll() {
                Ok(Async::Ready(Some(gossip))) => {
                    let sender_uid = gossip.sender_uid;
                    let (mut new_replies, mut new_hashes) = self.process_gossip(gossip);
                    replies.extend(new_replies.drain());
                    fetch_requests.entry(sender_uid).or_insert_with(|| HashSet::new())
                        .extend(new_hashes.drain());
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) => {
                    // End of the stream that feeds the gossips.
                    end_of_gossips = true;
                    break
                },
                Err(err) => error!("Failed to receive a gossip {:?}", err),
            }
        };

        // Collect new payloads
        let mut end_of_payloads = false;
        let mut payloads = P::new();
        loop {
            match self.payload_receiver.poll() {
                Ok(Async::Ready(Some(payload))) => payloads.union_update(payload),
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) => {
                    // End of the stream that feeds the payloads.
                    end_of_payloads = true;
                    break
                },
                Err(err) => error!("Failed to receive a payload {:?}", err),
            }
        }

        // Issue fetches, if required.
        for (receiver_uid, mut fetch_hashes) in fetch_requests.drain() {
            let reply = Gossip {
                sender_uid: self.owner_uid,
                receiver_uid,
                sender_sig: 0,  // TODO: Sign it.
                body: GossipBody::Fetch(fetch_hashes.drain().collect())
            };
            self.send_gossip(reply);
        }

        // Create a new root, if needed.
        if !payloads.is_empty() || self.dag_as_ref().has_dangling_roots() {
            let new_message = self.dag_as_mut().create_root_message(payloads, vec![]);
            let unsolicited = Gossip {
                sender_uid: self.owner_uid,
                receiver_uid: 0,  // TODO: Random receiver.
                sender_sig: 0,  // TODO: Sign it.
                body: GossipBody::Unsolicited(new_message.data.clone())
            };
            self.send_gossip(unsolicited);
        }

        // If the gossip stream and the payload stream are closed then we are done.
        if end_of_gossips && end_of_payloads {
            Ok(Async::Ready(None)) } else {
            Ok(Async::Ready(Some(()))) }
    }
}

impl<'a, P: Payload, W: WitnessSelector> Future for TxFlowTask<'a, P, W> {
    // This stream does not produce anything, it is meant to be run as a standalone task.
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Check if DAG needs to be created.
        if self.dag.is_none() {
            let witness_ptr = self.witness_selector.as_ref() as *const W;
            // Since we are controlling the creation of the DAG by encapsulating it here
            // this code is safe.
            self.dag = Some(Box::new(
                DAG::new(self.owner_uid, self.starting_epoch, unsafe {&*witness_ptr})));
        }

        loop {
            let res = self.messages_receiver.poll();
            let incoming_gossip = match res {
                Ok(Async::Ready(Some(gossip))) => gossip,
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(None)) => break,
                _ => break,
            };
            //self.messages_sender.send(incoming_gossip);
        }
        Ok(Async::Ready(()))
    }
}


#[cfg(test)]
mod tests {
    use tokio;

    use futures::sync::mpsc;
    use primitives::types::{UID, Gossip};
    use primitives::traits::WitnessSelector;
    use std::collections::{HashSet, HashMap};
    use futures::{Future, Poll, Async, Stream, Sink, stream};
    use futures::future::lazy;
    use rand::{thread_rng, Rng};

    use super::TxFlowTask;
    use testing_utils::FakePayload;

    struct FakeWitnessSelector {
        schedule: HashMap<u64, HashSet<UID>>,
    }

    impl FakeWitnessSelector {
        fn new() -> FakeWitnessSelector {
            FakeWitnessSelector {
                schedule: map!{
               0 => set!{0, 1, 2, 3}, 1 => set!{1, 2, 3, 4},
               2 => set!{2, 3, 4, 5}, 3 => set!{3, 4, 5, 6}}
            }
        }
    }

    impl WitnessSelector for FakeWitnessSelector {
        fn epoch_witnesses(&self, epoch: u64) -> &HashSet<u64> {
            self.schedule.get(&epoch).unwrap()
        }
        fn epoch_leader(&self, epoch: u64) -> UID {
            *self.epoch_witnesses(epoch).iter().min().unwrap()
        }
    }

    #[test]
    fn tmp() {
        return;
        let selector = FakeWitnessSelector::new();
        let (inc_gossip_tx, inc_gossip_rx) = mpsc::channel::<Gossip<FakePayload>>(1024);
        let (inc_payload_tx, inc_payload_rx) = mpsc::channel::<FakePayload>(1024);
        let (out_gossip_tx, out_gossip_rx) = mpsc::channel::<Gossip<FakePayload>>(1024);
        let task = TxFlowTask::new(0,0, inc_gossip_rx, inc_payload_rx, out_gossip_tx, selector);
        tokio::run(task);
    }

    fn print_type_of<T>(_: &T) {
        println!("{}", unsafe { std::intrinsics::type_name::<T>() });
    }


    use tokio::io;

    fn accumulator() {
        tokio::run(lazy(|| {
            let (inc_tx, inc_rx) = mpsc::channel(1_024);
            let (out_tx, out_rx) = mpsc::channel(1_024);



            tokio::spawn({
                stream::iter_ok(0..10).fold(inc_tx, |x, i| {
                    let tmp = x.send(format!("Emitted {}", i));

                    print_type_of(&tmp);
                    tmp.map_err(|e| println!("error = {:?}", e))
                })
                    .map(|_| ()) // Drop tx handle
            });

            tokio::spawn({
                inc_rx.fold(out_tx, |out_tx, m| {
                    out_tx.send(format!("Relayed `{}`", m))
                        .map_err(|e| println!("error = {:?}", e))
                }).map(|_| ())
            });


            tokio::spawn(
            out_rx.for_each(|msg| {
                println!("Finally `{}`", msg);
                Ok(())
            }));

            Ok(())
        }));
    }
    use std::time::Duration;
    use tokio::timer::Delay;

    pub const COOLDOWN: u64 = 1000;
    pub const FORCED_PING: u64 = 1500;

    use chrono::Local;

        struct Accumulator {
            inc_rx: mpsc::Receiver<i64>,
            out_tx: mpsc::Sender<i64>,
            cooldown_delay: Option<Delay>,
            forced_ping_delay: Option<Delay>,
            payload_buffer: Vec<i64>,
        }

    impl Accumulator {
        pub fn new(inc_rx: mpsc::Receiver<i64>, out_tx: mpsc::Sender<i64>) -> Self {
            Self {
                inc_rx,
                out_tx,
                cooldown_delay: None,
                forced_ping_delay: None,
                payload_buffer: vec![],
            }
        }
    }

    impl Stream for Accumulator {
        type Item = ();
        type Error = ();
        fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
            // Process incoming messages...
            loop {
                match self.inc_rx.poll() {
                    Ok(Async::Ready(Some(value))) => {
                        println!("{} Received value {}", Local::now().format("%M:%S.%f"), value);
                        self.payload_buffer.push(value)
                    },
                    Ok(Async::NotReady) => break,
                    Ok(Async::Ready(None)) => {
                        if self.payload_buffer.is_empty() {
                            return Ok(Async::Ready(None))
                        } else {
                            break;
                        }
                    },
                    Err(_) => {println!("ERR"); return Err(())},
                }
            }
            if self.payload_buffer.is_empty() {
                return Ok(Async::NotReady);
            }

            // .. but do not output them unless we pass the cooldown.
            if let Some(ref mut d) = self.cooldown_delay {
                try_ready!(d.poll().map_err(|_| ()));
            }
            println!("{} Cooldown is ok", Local::now().format("%M:%S.%f"));

            if self.payload_buffer.is_empty() {
                println!("buffer is 0");
                if let Some(ref mut d) = self.forced_ping_delay {
                    try_ready!(d.poll().map_err(|_| ()));
                }
                println!("{} But forced ping pushes us", Local::now().format("%M:%S.%f"));
            }

            let copied_out_tx = self.out_tx.clone();
            let now = std::time::Instant::now();
            self.cooldown_delay = Some(Delay::new(now + Duration::from_millis(COOLDOWN)));
            self.forced_ping_delay = Some(Delay::new(now + Duration::from_millis(FORCED_PING)));


            let acc: i64 = self.payload_buffer.iter().sum();
            println!("{} Relaying value {}", Local::now().format("%M:%S.%f"), acc);
            tokio::spawn(copied_out_tx.send(acc).map(|_|()).map_err(|e| {
                println!("Relaying error")
            }));
            self.payload_buffer.clear();
            Ok(Async::Ready(Some(())))
        }
    }

    impl Future for Accumulator {
        type Item = ();
        type Error = ();
        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            try_ready!(
             (self as &mut Stream<Item=Self::Item, Error=Self::Error>)
            .for_each(|_| Ok(())).poll());
            Ok(Async::Ready(()))
        }
    }

    #[test]
    fn my_accumulator() {
        tokio::run(lazy(|| {
            let (inc_tx, inc_rx) = mpsc::channel(1_024);
            let (out_tx, out_rx) = mpsc::channel(1_024);
            let mut acc = Accumulator::new(inc_rx, out_tx);
            tokio::spawn({
                let mut v: Vec<i64> = vec![];
                for i in 1..10 {

                    //v.push(r.abs() % 10);
                    v.push(1);
                }
                stream::iter_ok(v).fold(inc_tx, |inc_tx, el| {
                    let r: u64 = rand::random();
                    std::thread::sleep(Duration::from_millis(r % 300));
                    println!("{} Created {}", Local::now().format("%M:%S.%f"), el);
                    inc_tx.send(el).map_err(|_| ())
                }).map(|_|())
            });

            tokio::spawn(
                out_rx.for_each(|el| {
                    println!("{} Finally received {}", Local::now().format("%M:%S.%f"), el);
                    Ok(())
                })
            );
            tokio::spawn(acc);
            Ok(())
        }));
    }

}
