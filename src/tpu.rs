//! The `tpu` module implements the Transaction Processing Unit, a
//! 5-stage transaction processing pipeline in software.
//!
//! ```text
//!             .---------------------------------------------------------------.
//!             |  TPU                                     .-----.              |
//!             |                                          | PoH |              |
//!             |                                          `--+--`              |
//!             |                                             |                 |
//!             |                                             v                 |
//!             |  .-------.  .-----------.  .---------.  .--------.  .-------. |
//! .---------. |  | Fetch |  | SigVerify |  | Banking |  | Record |  | Write | |  .------------.
//! | Clients |--->| Stage |->|   Stage   |->|  Stage  |->| Stage  |->| Stage +--->| Validators |
//! `---------` |  |       |  |           |  |         |  |        |  |       | |  `------------`
//!             |  `-------`  `-----------`  `----+----`  `--------`  `---+---` |
//!             |                                 |                       |     |
//!             |                                 |                       |     |
//!             |                                 |                       |     |
//!             |                                 |                       |     |
//!             `---------------------------------|-----------------------|-----`
//!                                               |                       |
//!                                               v                       v
//!                                            .------.               .--------.
//!                                            | Bank |               | Ledger |
//!                                            `------`               `--------`
//! ```

use bank::Bank;
use banking_stage::BankingStage;
use crdt::Crdt;
use fetch_stage::FetchStage;
use packet::{BlobRecycler, PacketRecycler};
use record_stage::RecordStage;
use service::Service;
use signature::Keypair;
use sigverify_stage::SigVerifyStage;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use streamer::BlobReceiver;
use write_stage::WriteStage;

pub struct Tpu {
    fetch_stage: FetchStage,
    sigverify_stage: SigVerifyStage,
    banking_stage: BankingStage,
    record_stage: RecordStage,
    write_stage: WriteStage,
}

impl Tpu {
    pub fn new(
        keypair: Keypair,
        bank: &Arc<Bank>,
        crdt: &Arc<RwLock<Crdt>>,
        tick_duration: Option<Duration>,
        transactions_socket: UdpSocket,
        blob_recycler: &BlobRecycler,
        exit: Arc<AtomicBool>,
        ledger_path: &str,
        sigverify_disabled: bool,
    ) -> (Self, BlobReceiver) {
        let packet_recycler = PacketRecycler::default();

        let (fetch_stage, packet_receiver) =
            FetchStage::new(transactions_socket, exit, &packet_recycler);

        let (sigverify_stage, verified_receiver) =
            SigVerifyStage::new(packet_receiver, sigverify_disabled);

        let (banking_stage, signal_receiver) =
            BankingStage::new(bank.clone(), verified_receiver, packet_recycler.clone());

        let (record_stage, entry_receiver) = match tick_duration {
            Some(tick_duration) => {
                RecordStage::new_with_clock(signal_receiver, &bank.last_id(), tick_duration)
            }
            None => RecordStage::new(signal_receiver, &bank.last_id()),
        };

        let (write_stage, blob_receiver) = WriteStage::new(
            keypair,
            bank.clone(),
            crdt.clone(),
            blob_recycler.clone(),
            ledger_path,
            entry_receiver,
        );

        let tpu = Tpu {
            fetch_stage,
            sigverify_stage,
            banking_stage,
            record_stage,
            write_stage,
        };
        (tpu, blob_receiver)
    }

    pub fn close(self) -> thread::Result<()> {
        self.fetch_stage.close();
        self.join()
    }
}

impl Service for Tpu {
    fn thread_hdls(self) -> Vec<JoinHandle<()>> {
        let mut thread_hdls = vec![];
        thread_hdls.extend(self.fetch_stage.thread_hdls().into_iter());
        thread_hdls.extend(self.sigverify_stage.thread_hdls().into_iter());
        thread_hdls.extend(self.banking_stage.thread_hdls().into_iter());
        thread_hdls.extend(self.record_stage.thread_hdls().into_iter());
        thread_hdls.extend(self.write_stage.thread_hdls().into_iter());
        thread_hdls
    }

    fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls() {
            thread_hdl.join()?;
        }
        Ok(())
    }
}
