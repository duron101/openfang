//! Offline Intelligence Report Queue — caches reports during communication outages.
//! Uses SQLite for durability with SHA256 deduplication.

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReportPriority {
    Normal = 0,
    Urgent = 1,
    Critical = 2,
}

#[derive(Debug, Clone)]
pub struct PendingReport {
    pub id: i64,
    pub report_type: String,
    pub payload_json: String,
    pub priority: ReportPriority,
    pub created_at: String,
}

pub struct ReportQueue {
    conn: Arc<Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub attempted: u32,
    pub synced: u32,
    pub failed: Vec<(i64, String)>,
}

fn dedup_hash(report_type: &str, payload_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(report_type.as_bytes());
    hasher.update([0xff]);
    hasher.update(payload_json.as_bytes());
    hex::encode(hasher.finalize())
}

impl ReportQueue {
    pub fn new(conn: Arc<Connection>) -> Self {
        Self { conn }
    }

    pub fn enqueue(
        &self,
        report_type: &str,
        payload_json: &str,
        priority: ReportPriority,
    ) -> Result<bool, String> {
        let hash = dedup_hash(report_type, payload_json);
        let result = self.conn.execute(
            "INSERT OR IGNORE INTO pending_reports (report_type, payload_json, priority, created_at, synced, dedup_hash)
             VALUES (?1, ?2, ?3, datetime('now'), 0, ?4)",
            rusqlite::params![report_type, payload_json, priority as i32, hash],
        );
        match result {
            Ok(rows) => Ok(rows > 0),
            Err(e) => Err(format!("enqueue: {e}")),
        }
    }

    pub fn pending(&self) -> Result<Vec<PendingReport>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, report_type, payload_json, priority, created_at
                 FROM pending_reports WHERE synced = 0 ORDER BY priority DESC, created_at ASC",
            )
            .map_err(|e| format!("query: {e}"))?;
        let reports = stmt
            .query_map([], |row| {
                Ok(PendingReport {
                    id: row.get(0)?,
                    report_type: row.get(1)?,
                    payload_json: row.get(2)?,
                    priority: match row.get::<_, i32>(3)? {
                        1 => ReportPriority::Urgent,
                        2 => ReportPriority::Critical,
                        _ => ReportPriority::Normal,
                    },
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| format!("row: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(reports)
    }

    pub fn mark_synced(&self, ids: &[i64]) -> Result<usize, String> {
        let mut count = 0;
        for id in ids {
            count += self
                .conn
                .execute(
                    "UPDATE pending_reports SET synced = 1, synced_at = datetime('now') WHERE id = ?1 AND synced = 0",
                    rusqlite::params![id],
                )
                .map_err(|e| format!("mark: {e}"))?;
        }
        Ok(count)
    }

    pub fn pending_count(&self) -> Result<u32, String> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM pending_reports WHERE synced = 0",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("count: {e}"))
    }

    /// Deliver pending reports through `deliver` and only mark rows synced after
    /// the remote side acknowledges success. Failed rows remain pending.
    pub fn sync_pending<F>(&self, mut deliver: F) -> Result<SyncReport, String>
    where
        F: FnMut(&PendingReport) -> Result<(), String>,
    {
        let pending = self.pending()?;
        let mut synced = 0u32;
        let mut failed = Vec::new();

        for report in &pending {
            match deliver(report) {
                Ok(()) => {
                    synced += self.mark_synced(&[report.id])? as u32;
                }
                Err(e) => failed.push((report.id, e)),
            }
        }

        Ok(SyncReport {
            attempted: pending.len() as u32,
            synced,
            failed,
        })
    }

    /// Compatibility helper for tests/admin tools that have already delivered
    /// all pending reports out-of-band. Production sync should use `sync_pending`
    /// so a failed send cannot clear the offline cache.
    pub fn sync_all(&self) -> Result<u32, String> {
        let pending = self.pending()?;
        let ids: Vec<i64> = pending.iter().map(|r| r.id).collect();
        Ok(self.mark_synced(&ids)? as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[allow(clippy::arc_with_non_send_sync)] // single-threaded in-memory test DB
    fn setup() -> (Arc<Connection>, ReportQueue) {
        let conn = Arc::new(Connection::open_in_memory().unwrap());
        conn.execute_batch(
            "CREATE TABLE pending_reports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                report_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0,
                synced_at TEXT,
                dedup_hash TEXT NOT NULL,
                UNIQUE(dedup_hash)
            );",
        )
        .unwrap();
        let queue = ReportQueue::new(Arc::clone(&conn));
        (conn, queue)
    }

    #[test]
    fn test_enqueue_and_pending() {
        let (_, queue) = setup();
        assert!(queue
            .enqueue("contact", r#"{"lat":30}"#, ReportPriority::Normal)
            .unwrap());
        let pending = queue.pending().unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn test_dedup() {
        let (_, queue) = setup();
        assert!(queue
            .enqueue("contact", "x", ReportPriority::Normal)
            .unwrap());
        assert!(!queue
            .enqueue("contact", "x", ReportPriority::Normal)
            .unwrap());
        assert!(queue
            .enqueue("health", "x", ReportPriority::Normal)
            .unwrap());
    }

    #[test]
    fn test_mark_synced() {
        let (_, queue) = setup();
        queue
            .enqueue("a", r#"{"id":"a"}"#, ReportPriority::Normal)
            .unwrap();
        queue
            .enqueue("b", r#"{"id":"b"}"#, ReportPriority::Critical)
            .unwrap();
        let pending = queue.pending().unwrap();
        assert_eq!(
            pending.len(),
            2,
            "expected two distinct reports after enqueue"
        );
        queue.mark_synced(&[pending[0].id]).unwrap();
        assert_eq!(queue.pending_count().unwrap(), 1);
    }

    #[test]
    fn test_sync_all() {
        let (_, queue) = setup();
        queue.enqueue("a", "1", ReportPriority::Normal).unwrap();
        queue.enqueue("b", "2", ReportPriority::Urgent).unwrap();
        assert_eq!(queue.sync_all().unwrap(), 2);
        assert_eq!(queue.pending_count().unwrap(), 0);
    }

    /// Phase 6 — communication-interruption recovery.
    ///
    /// Models a full outage→reconnect cycle: reports accumulate while comms are
    /// down, drain in priority order on reconnect, and incremental re-sync is
    /// idempotent (already-delivered reports are never sent twice).
    #[test]
    fn test_comm_outage_recovery_is_priority_ordered_and_idempotent() {
        let (_, queue) = setup();

        // ── Comms down: contacts buffer locally, no sync possible ──
        queue
            .enqueue("contact", r#"{"id":"c-normal"}"#, ReportPriority::Normal)
            .unwrap();
        queue
            .enqueue(
                "contact",
                r#"{"id":"c-critical"}"#,
                ReportPriority::Critical,
            )
            .unwrap();
        queue
            .enqueue("contact", r#"{"id":"c-urgent"}"#, ReportPriority::Urgent)
            .unwrap();

        // Duplicate critical sighting during the outage is deduped, not re-buffered.
        assert!(!queue
            .enqueue(
                "contact",
                r#"{"id":"c-critical"}"#,
                ReportPriority::Critical
            )
            .unwrap());
        assert_eq!(queue.pending_count().unwrap(), 3);

        // Highest priority drains first so the C2 link carries the most important
        // intelligence when bandwidth is scarce.
        let pending = queue.pending().unwrap();
        assert_eq!(pending[0].priority, ReportPriority::Critical);
        assert_eq!(pending[1].priority, ReportPriority::Urgent);
        assert_eq!(pending[2].priority, ReportPriority::Normal);

        // ── Reconnect attempt: one delivery fails, so only acked rows clear ──
        let first_sync = queue
            .sync_pending(|report| {
                if report.payload_json.contains("c-urgent") {
                    return Err("link dropped mid-drain".into());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(first_sync.attempted, 3);
        assert_eq!(first_sync.synced, 2);
        assert_eq!(first_sync.failed.len(), 1);
        assert_eq!(queue.pending_count().unwrap(), 1);

        let remaining = queue.pending().unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].payload_json.contains("c-urgent"));

        // ── Reconnect stabilized: remaining row is acked and drained ──
        let second_sync = queue.sync_pending(|_| Ok(())).unwrap();
        assert_eq!(second_sync.attempted, 1);
        assert_eq!(second_sync.synced, 1);
        assert!(second_sync.failed.is_empty());
        assert_eq!(queue.pending_count().unwrap(), 0);

        // Re-enqueuing an already-delivered payload is blocked by dedup, so a
        // flaky link that retries the same report does not double-deliver.
        assert!(!queue
            .enqueue(
                "contact",
                r#"{"id":"c-critical"}"#,
                ReportPriority::Critical
            )
            .unwrap());
        assert_eq!(queue.pending_count().unwrap(), 0);

        // A genuinely new report after recovery syncs on its own (incremental).
        assert!(queue
            .enqueue(
                "contact",
                r#"{"id":"c-post-recovery"}"#,
                ReportPriority::Urgent
            )
            .unwrap());
        assert_eq!(queue.pending_count().unwrap(), 1);
        assert_eq!(queue.sync_pending(|_| Ok(())).unwrap().synced, 1);
        assert_eq!(queue.pending_count().unwrap(), 0);
    }
}
