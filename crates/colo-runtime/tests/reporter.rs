use colo_runtime::{
    AccountIdentity, Intent, Journal, JournalRecord, OrderType, Reporter, Reservation, Side,
    Submission, TimeInForce,
};
use tempfile::TempDir;
use uuid::Uuid;

fn identity() -> AccountIdentity {
    AccountIdentity {
        account_id: Uuid::new_v4(),
        venue: "BINANCE".into(),
        environment: "LIVE".into(),
        external_account_ref: "sub".into(),
    }
}

fn record(id: &str, sequence: u64) -> JournalRecord {
    JournalRecord::Intent {
        submission: Submission { client_order_id: id.into(), sequence },
        intent: Intent {
            instrument: "BTCUSDT".into(),
            side: Side::Buy,
            quantity: 1.0,
            order_type: OrderType::Limit,
            limit_price: Some(100.0),
            max_quote_spend: None,
            time_in_force: TimeInForce::Gtc,
        },
        reservation: Reservation { asset: "USDT".into(), amount: 100.0 },
    }
}

#[test]
fn replay_survives_reconnect_until_the_ack_is_durable() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let mut journal = Journal::open(&dir.path().join("journal"), &id).unwrap();
    journal.append_durable(&record("a", 1)).unwrap();
    journal.append_durable(&record("b", 2)).unwrap();
    let mut reporter = Reporter::new(id.clone(), 1, "n1", 1, &journal).unwrap();
    let first = reporter.next_batch(&journal, 128).unwrap().unwrap();
    assert_eq!(first.records.len(), 2);
    reporter.reconnect("n2");
    let replay = reporter.next_batch(&journal, 128).unwrap().unwrap();
    assert_eq!(replay.records.len(), 2, "unacked events were lost on reconnect");
    reporter.ack(&mut journal, replay.last_sequence()).unwrap();
    assert!(journal.unacked(1, 10).unwrap().is_empty());
    assert!(reporter.next_batch(&journal, 128).unwrap().is_none());
    let mut restarted = Reporter::new(id.clone(), 1, "n3", 1, &journal).unwrap();
    assert!(restarted.next_batch(&journal, 128).unwrap().is_none());
}

#[test]
fn a_batch_is_bounded_to_the_protocol_record_limit() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let mut journal = Journal::open(&dir.path().join("journal"), &id).unwrap();
    for i in 0..200 {
        journal.append_durable(&record(&format!("c{i}"), i + 1)).unwrap();
    }
    let mut reporter = Reporter::new(id, 1, "n", 1, &journal).unwrap();
    let batch = reporter.next_batch(&journal, 10_000).unwrap().unwrap();
    assert!(batch.records.len() <= 128);
}
