//! The hard spend cap — the CLAUDE.md guardrail as code.
//!
//! [`CappedFrontier`] wraps any [`Frontier`] and refuses, BEFORE anything is
//! sent, any request whose worst case could take the session past its cap.
//! Every call that does go out is appended to the ledger before the response
//! is released to the caller; a ledger append failure withholds the response
//! (an unrecorded paid call is exactly the state the guardrail forbids).
//!
//! Fail-closed: a cap of 0 — the default everywhere a cap is read — refuses
//! every paid call. Owner authorization is expressed by passing a nonzero cap
//! explicitly ($25/session authorized 2026-07-04; ADR-0010).

use sha2::{Digest, Sha256};

use crate::ledger::{LedgerEntry, SpendLedger, now_unix_ms};
use crate::prices::{ModelPrice, price_of};
use crate::{Frontier, FrontierError, FrontierRequest, FrontierResponse};

/// A spend-capped, ledgered frontier client. See the module docs; construct
/// one per session/purpose and hand it to CEGIS or tier-0 as `&mut dyn
/// Frontier`.
#[derive(Debug)]
pub struct CappedFrontier<C: Frontier> {
    inner: C,
    cap_usd_micros: u64,
    session: String,
    purpose: String,
    ledger: SpendLedger,
}

impl<C: Frontier> CappedFrontier<C> {
    /// Wrap `inner` under `cap_usd_micros` for one `session`/`purpose`.
    /// The inner client's model must be in the pinned price table — an
    /// unknown model is refused at construction, before any call site can
    /// reach it (a cap over an unpriceable model is unenforceable).
    pub fn new(
        inner: C,
        cap_usd_micros: u64,
        session: &str,
        purpose: &str,
        ledger: SpendLedger,
    ) -> Result<Self, FrontierError> {
        price_of(inner.model_id())?;
        Ok(Self {
            inner,
            cap_usd_micros,
            session: session.to_owned(),
            purpose: purpose.to_owned(),
            ledger,
        })
    }

    /// µ$ this session has already spent, from the ledger (the single source
    /// of truth — never an in-memory counter that a crash could zero).
    pub fn session_spent_usd_micros(&self) -> Result<u64, FrontierError> {
        self.ledger.session_total_usd_micros(&self.session)
    }
}

/// Conservative worst case for one request, in µ$: input estimated at one
/// token per TWO bytes of prompt (a ~2x margin over the ~4-chars/token rule
/// of thumb for English) plus the full `max_output_tokens` at output price.
/// Over-estimating only refuses earlier — the fail-closed direction.
fn worst_case_usd_micros(price: &ModelPrice, request: &FrontierRequest) -> u64 {
    let prompt_bytes = (request.system.len() + request.user.len()) as u64;
    let input_estimate = prompt_bytes.div_ceil(2);
    price
        .input_cost(input_estimate)
        .saturating_add(price.output_cost(u64::from(request.max_output_tokens)))
}

/// sha256 hex of the canonical request JSON — the ledger's provenance key.
fn request_digest(request: &FrontierRequest) -> String {
    let canonical = serde_json::to_string(request).expect("request serialization cannot fail");
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl<C: Frontier> Frontier for CappedFrontier<C> {
    fn complete(&mut self, request: &FrontierRequest) -> Result<FrontierResponse, FrontierError> {
        let price = price_of(self.inner.model_id())?;
        let spent = self.ledger.session_total_usd_micros(&self.session)?;
        let estimated = worst_case_usd_micros(price, request);
        if spent.saturating_add(estimated) > self.cap_usd_micros {
            return Err(FrontierError::CapExceeded {
                spent_usd_micros: spent,
                estimated_usd_micros: estimated,
                cap_usd_micros: self.cap_usd_micros,
            });
        }

        let response = self.inner.complete(request)?;

        // ledger first, response second: the recorded spend is the response's
        // own accounting (providers compute cost from the same pinned table)
        let entry = LedgerEntry {
            ts_unix_ms: now_unix_ms(),
            session: self.session.clone(),
            model: response.model.clone(),
            purpose: self.purpose.clone(),
            input_tokens: response.input_tokens,
            output_tokens: response.output_tokens,
            cost_usd_micros: response.cost_usd_micros,
            request_digest: request_digest(request),
        };
        self.ledger.append(&entry)?;
        Ok(response)
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedFrontier;

    const MODEL: &str = "gpt-5.4-mini"; // pinned: $0.75/$4.50 per MTok

    fn request(max_output_tokens: u32) -> FrontierRequest {
        FrontierRequest {
            system: "s".repeat(100),
            user: "u".repeat(100),
            max_output_tokens,
        }
    }

    fn capped(
        script: ScriptedFrontier,
        cap: u64,
        dir: &std::path::Path,
    ) -> CappedFrontier<ScriptedFrontier> {
        let ledger = SpendLedger::new(dir.join("spend.jsonl"));
        CappedFrontier::new(script, cap, "test-session", "test", ledger).expect("pinned model")
    }

    #[test]
    fn under_cap_call_goes_through_and_lands_in_the_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut inner = ScriptedFrontier::new(MODEL);
        inner.push_text("answer one", 120, 40, 300);
        inner.push_text("answer two", 130, 50, 350);
        let mut client = capped(inner, 1_000_000, dir.path()); // $1 cap

        let first = client.complete(&request(64)).expect("under cap");
        assert_eq!(first.text, "answer one");
        assert_eq!(client.session_spent_usd_micros().expect("spent"), 300);

        client.complete(&request(64)).expect("still under cap");
        assert_eq!(client.session_spent_usd_micros().expect("spent"), 650);

        let ledger = SpendLedger::new(dir.path().join("spend.jsonl"));
        let entries = ledger.read_all().expect("ledger parses");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].purpose, "test");
        assert_eq!(entries[0].model, MODEL);
        assert_eq!(entries[0].request_digest.len(), 64);
    }

    #[test]
    fn cap_zero_refuses_before_sending_anything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut client = capped(ScriptedFrontier::new(MODEL), 0, dir.path());

        let err = client.complete(&request(64)).expect_err("cap 0 refuses");
        assert!(matches!(
            err,
            FrontierError::CapExceeded {
                cap_usd_micros: 0,
                ..
            }
        ));
        // nothing was sent and nothing was ledgered
        assert_eq!(client.inner.requests.len(), 0);
        assert_eq!(client.session_spent_usd_micros().expect("spent"), 0);
    }

    #[test]
    fn worst_case_output_alone_can_refuse() {
        let dir = tempfile::tempdir().expect("tempdir");
        // cap 10_000µ$ = $0.01; 10_000 output tokens at $4.50/MTok worst-case
        // 45_000µ$ — refused unsent even though nothing is spent yet
        let mut client = capped(ScriptedFrontier::new(MODEL), 10_000, dir.path());
        let err = client
            .complete(&request(10_000))
            .expect_err("worst case exceeds cap");
        match err {
            FrontierError::CapExceeded {
                spent_usd_micros,
                estimated_usd_micros,
                ..
            } => {
                assert_eq!(spent_usd_micros, 0);
                assert!(estimated_usd_micros >= 45_000, "{estimated_usd_micros}");
            }
            other => panic!("expected CapExceeded, got {other}"),
        }
        assert_eq!(client.inner.requests.len(), 0);
    }

    #[test]
    fn spend_accumulates_until_the_cap_refuses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut inner = ScriptedFrontier::new(MODEL);
        // first call ledgers 40_000µ$; the second call's worst case (~363µ$:
        // 100 estimated input tokens + 64 output tokens at mini prices) then
        // crosses the 40_200µ$ cap
        inner.push_text("a", 10, 10, 40_000);
        inner.push_text("never reached", 10, 10, 40_000);
        let mut client = capped(inner, 40_200, dir.path());

        client.complete(&request(64)).expect("first call fits");
        let err = client
            .complete(&request(64))
            .expect_err("second call would cross the cap");
        assert!(matches!(
            err,
            FrontierError::CapExceeded {
                spent_usd_micros: 40_000,
                ..
            }
        ));
        assert_eq!(client.inner.requests.len(), 1, "the refusal was unsent");
    }

    #[test]
    fn unpinned_model_is_refused_at_construction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = SpendLedger::new(dir.path().join("spend.jsonl"));
        let err = CappedFrontier::new(
            ScriptedFrontier::new("mystery-model"),
            1_000_000,
            "s",
            "p",
            ledger,
        )
        .expect_err("unpriceable model cannot be capped");
        assert!(matches!(err, FrontierError::UnknownModel { .. }));
    }

    #[test]
    fn unreadable_ledger_refuses_before_sending() {
        // a corrupt ledger means the running total cannot be trusted, so the
        // call is refused UNSENT — deterministic on every platform (a prior
        // path-through-a-file variant of this test flipped between pre- and
        // post-send by OS error kind: NotFound on Windows, ENOTDIR on Linux)
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spend.jsonl");
        std::fs::write(&path, "this is not a ledger line\n").expect("seed corrupt ledger");

        let mut inner = ScriptedFrontier::new(MODEL);
        inner.push_text("never returned", 10, 10, 100);
        let mut client = CappedFrontier::new(inner, 1_000_000, "s", "p", SpendLedger::new(&path))
            .expect("pinned model");

        let err = client
            .complete(&request(64))
            .expect_err("uncountable spend must refuse");
        assert!(matches!(err, FrontierError::Ledger { .. }));
        assert_eq!(
            client.inner.requests.len(),
            0,
            "refused pre-send: the totals were unreadable"
        );
    }

    #[test]
    fn ledger_append_failure_withholds_the_response() {
        // a READABLE ledger whose APPEND fails: seed one valid entry, then
        // make the file read-only — the pre-send total check passes, the call
        // goes out, and the unrecordable response is withheld
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spend.jsonl");
        let ledger = SpendLedger::new(&path);
        ledger
            .append(&LedgerEntry {
                ts_unix_ms: 1_720_000_000_000,
                session: "s".to_owned(),
                model: MODEL.to_owned(),
                purpose: "seed".to_owned(),
                input_tokens: 1,
                output_tokens: 1,
                cost_usd_micros: 0,
                request_digest: "seed".to_owned(),
            })
            .expect("seed a valid line");
        let mut readonly = std::fs::metadata(&path).expect("metadata").permissions();
        readonly.set_readonly(true);
        std::fs::set_permissions(&path, readonly.clone()).expect("make ledger read-only");

        let mut inner = ScriptedFrontier::new(MODEL);
        inner.push_text("paid answer", 10, 10, 100);
        let mut client =
            CappedFrontier::new(inner, 1_000_000, "s", "p", ledger).expect("pinned model");

        let err = client
            .complete(&request(64))
            .expect_err("unledgerable call must not return the response");
        assert!(matches!(err, FrontierError::Ledger { .. }));
        // the call DID go out (the provider was paid) — the error is loud
        // precisely because the recording, not the call, failed
        assert_eq!(client.inner.requests.len(), 1);

        // test cleanup only: restore writability inside the private tempdir so
        // its removal succeeds on Windows (read-only files refuse deletion)
        #[allow(clippy::permissions_set_readonly_false)]
        {
            readonly.set_readonly(false);
            let _ = std::fs::set_permissions(&path, readonly);
        }
    }
}
