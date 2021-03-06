//! The `tvu` module implements the Transaction Validation Unit, a
//! 3-stage transaction validation pipeline in software.
//!
//! ```text
//!      .------------------------------------------------.
//!      |                                                |
//!      |           .------------------------------------+------------.
//!      |           |  TVU                               |            |
//!      |           |                                    |            |
//!      |           |                                    |            |  .------------.
//!      |           |                   .----------------+-------------->| Validators |
//!      v           |  .-------.        |                |            |  `------------`
//! .----+---.       |  |       |   .----+-------.   .----+---------.  |
//! | Leader |--------->| Blob  |   | Retransmit |   | Replicate    |  |
//! `--------`       |  | Fetch |-->|   Stage    |-->| Stage /      |  |
//! .------------.   |  | Stage |   |            |   | Vote Stage   |  |
//! | Validators |----->|       |   `------------`   `----+---------`  |
//! `------------`   |  `-------`                         |            |
//!                  |                                    |            |
//!                  |                                    |            |
//!                  |                                    |            |
//!                  `------------------------------------|------------`
//!                                                       |
//!                                                       v
//!                                                    .------.
//!                                                    | Bank |
//!                                                    `------`
//! ```
//!
//! 1. Fetch Stage
//! - Incoming blobs are picked up from the replicate socket and repair socket.
//! 2. SharedWindow Stage
//! - Blobs are windowed until a contiguous chunk is available.  This stage also repairs and
//! retransmits blobs that are in the queue.
//! 3. Replicate Stage
//! - Transactions in blobs are processed and applied to the bank.
//! - TODO We need to verify the signatures in the blobs.

use bank::Bank;
use blob_fetch_stage::BlobFetchStage;
use crdt::Crdt;
use packet::BlobRecycler;
use replicate_stage::ReplicateStage;
use retransmit_stage::RetransmitStage;
use service::Service;
use signature::Keypair;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use window::SharedWindow;

pub struct Tvu {
    replicate_stage: ReplicateStage,
    fetch_stage: BlobFetchStage,
    retransmit_stage: RetransmitStage,
}

impl Tvu {
    /// This service receives messages from a leader in the network and processes the transactions
    /// on the bank state.
    /// # Arguments
    /// * `bank` - The bank state.
    /// * `entry_height` - Initial ledger height, passed to replicate stage
    /// * `crdt` - The crdt state.
    /// * `window` - The window state.
    /// * `replicate_socket` - my replicate socket
    /// * `repair_socket` - my repair socket
    /// * `retransmit_socket` - my retransmit socket
    /// * `exit` - The exit signal.
    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn new(
        keypair: Keypair,
        bank: &Arc<Bank>,
        entry_height: u64,
        crdt: Arc<RwLock<Crdt>>,
        window: SharedWindow,
        replicate_socket: UdpSocket,
        repair_socket: UdpSocket,
        retransmit_socket: UdpSocket,
        ledger_path: Option<&str>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let blob_recycler = BlobRecycler::default();
        let (fetch_stage, blob_fetch_receiver) = BlobFetchStage::new_multi_socket(
            vec![replicate_socket, repair_socket],
            exit.clone(),
            &blob_recycler,
        );
        //TODO
        //the packets coming out of blob_receiver need to be sent to the GPU and verified
        //then sent to the window, which does the erasure coding reconstruction
        let (retransmit_stage, blob_window_receiver) = RetransmitStage::new(
            &crdt,
            window,
            entry_height,
            retransmit_socket,
            &blob_recycler,
            blob_fetch_receiver,
        );

        let replicate_stage = ReplicateStage::new(
            keypair,
            bank.clone(),
            crdt,
            blob_recycler,
            blob_window_receiver,
            ledger_path,
            exit,
        );

        Tvu {
            replicate_stage,
            fetch_stage,
            retransmit_stage,
        }
    }

    pub fn close(self) -> thread::Result<()> {
        self.fetch_stage.close();
        self.join()
    }
}

impl Service for Tvu {
    fn thread_hdls(self) -> Vec<JoinHandle<()>> {
        let mut thread_hdls = vec![];
        thread_hdls.extend(self.replicate_stage.thread_hdls().into_iter());
        thread_hdls.extend(self.fetch_stage.thread_hdls().into_iter());
        thread_hdls.extend(self.retransmit_stage.thread_hdls().into_iter());
        thread_hdls
    }

    fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls() {
            thread_hdl.join()?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use bank::Bank;
    use bincode::serialize;
    use crdt::{Crdt, TestNode};
    use entry::Entry;
    use hash::{hash, Hash};
    use logger;
    use mint::Mint;
    use ncp::Ncp;
    use packet::BlobRecycler;
    use result::Result;
    use service::Service;
    use signature::{Keypair, KeypairUtil};
    use std::collections::VecDeque;
    use std::net::UdpSocket;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use streamer;
    use transaction::Transaction;
    use tvu::Tvu;
    use window::{self, SharedWindow};

    fn new_ncp(
        crdt: Arc<RwLock<Crdt>>,
        listen: UdpSocket,
        exit: Arc<AtomicBool>,
    ) -> Result<(Ncp, SharedWindow)> {
        let window = window::default_window();
        let send_sock = UdpSocket::bind("0.0.0.0:0").expect("bind 0");
        let ncp = Ncp::new(&crdt, window.clone(), None, listen, send_sock, exit)?;
        Ok((ncp, window))
    }

    /// Test that message sent from leader to target1 and replicated to target2
    #[test]
    fn test_replicate() {
        logger::setup();
        let leader = TestNode::new_localhost();
        let target1_keypair = Keypair::new();
        let target1 = TestNode::new_localhost_with_pubkey(target1_keypair.pubkey());
        let target2 = TestNode::new_localhost();
        let exit = Arc::new(AtomicBool::new(false));

        //start crdt_leader
        let mut crdt_l = Crdt::new(leader.data.clone()).expect("Crdt::new");
        crdt_l.set_leader(leader.data.id);

        let cref_l = Arc::new(RwLock::new(crdt_l));
        let dr_l = new_ncp(cref_l, leader.sockets.gossip, exit.clone()).unwrap();

        //start crdt2
        let mut crdt2 = Crdt::new(target2.data.clone()).expect("Crdt::new");
        crdt2.insert(&leader.data);
        crdt2.set_leader(leader.data.id);
        let leader_id = leader.data.id;
        let cref2 = Arc::new(RwLock::new(crdt2));
        let dr_2 = new_ncp(cref2, target2.sockets.gossip, exit.clone()).unwrap();

        // setup some blob services to send blobs into the socket
        // to simulate the source peer and get blobs out of the socket to
        // simulate target peer
        let recv_recycler = BlobRecycler::default();
        let resp_recycler = BlobRecycler::default();
        let (s_reader, r_reader) = channel();
        let t_receiver = streamer::blob_receiver(
            exit.clone(),
            recv_recycler.clone(),
            target2.sockets.replicate,
            s_reader,
        ).unwrap();

        // simulate leader sending messages
        let (s_responder, r_responder) = channel();
        let t_responder = streamer::responder(
            "test_replicate",
            leader.sockets.requests,
            resp_recycler.clone(),
            r_responder,
        );

        let starting_balance = 10_000;
        let mint = Mint::new(starting_balance);
        let replicate_addr = target1.data.contact_info.tvu;
        let bank = Arc::new(Bank::new(&mint));

        //start crdt1
        let mut crdt1 = Crdt::new(target1.data.clone()).expect("Crdt::new");
        crdt1.insert(&leader.data);
        crdt1.set_leader(leader.data.id);
        let cref1 = Arc::new(RwLock::new(crdt1));
        let dr_1 = new_ncp(cref1.clone(), target1.sockets.gossip, exit.clone()).unwrap();

        let tvu = Tvu::new(
            target1_keypair,
            &bank,
            0,
            cref1,
            dr_1.1,
            target1.sockets.replicate,
            target1.sockets.repair,
            target1.sockets.retransmit,
            None,
            exit.clone(),
        );

        let mut alice_ref_balance = starting_balance;
        let mut msgs = VecDeque::new();
        let mut cur_hash = Hash::default();
        let mut blob_id = 0;
        let num_transfers = 10;
        let transfer_amount = 501;
        let bob_keypair = Keypair::new();
        for i in 0..num_transfers {
            let entry0 = Entry::new(&cur_hash, i, vec![], false);
            bank.register_entry_id(&cur_hash);
            cur_hash = hash(&cur_hash.as_ref());

            let tx0 = Transaction::new(
                &mint.keypair(),
                bob_keypair.pubkey(),
                transfer_amount,
                cur_hash,
            );
            bank.register_entry_id(&cur_hash);
            cur_hash = hash(&cur_hash.as_ref());
            let entry1 = Entry::new(&cur_hash, i + num_transfers, vec![tx0], false);
            bank.register_entry_id(&cur_hash);
            cur_hash = hash(&cur_hash.as_ref());

            alice_ref_balance -= transfer_amount;

            for entry in vec![entry0, entry1] {
                let b = resp_recycler.allocate();
                {
                    let mut w = b.write().unwrap();
                    w.set_index(blob_id).unwrap();
                    blob_id += 1;
                    w.set_id(leader_id).unwrap();

                    let serialized_entry = serialize(&entry).unwrap();

                    w.data_mut()[..serialized_entry.len()].copy_from_slice(&serialized_entry);
                    w.set_size(serialized_entry.len());
                    w.meta.set_addr(&replicate_addr);
                }
                msgs.push_back(b);
            }
        }

        // send the blobs into the socket
        s_responder.send(msgs).expect("send");
        drop(s_responder);

        // receive retransmitted messages
        let timer = Duration::new(1, 0);
        while let Ok(msg) = r_reader.recv_timeout(timer) {
            trace!("msg: {:?}", msg);
        }

        let alice_balance = bank.get_balance(&mint.keypair().pubkey());
        assert_eq!(alice_balance, alice_ref_balance);

        let bob_balance = bank.get_balance(&bob_keypair.pubkey());
        assert_eq!(bob_balance, starting_balance - alice_ref_balance);

        tvu.close().expect("close");
        dr_l.0.join().expect("join");
        dr_2.0.join().expect("join");
        dr_1.0.join().expect("join");
        t_receiver.join().expect("join");
        t_responder.join().expect("join");
    }
}
