//! Testnet faucet PoW v3 client (e2e gas autonomy).
//!
//! Implements <https://faucet.sui.io/pow-spec/pow-spec.md> version 1 so CI and
//! live sessions can refill the e2e signer's balance instead of bailing with a
//! manual "visit the faucet" step. Conformance is pinned by the official
//! vectors (`fixtures/pow-vectors.json`): exact preimage join, Argon2d outputs,
//! and the 128-bit threshold rule — no live faucet needed to verify the math.
//!
//! Payout semantics follow the spec's recovery rules: retryable codes
//! (`stale_checkpoint`, `overloaded`, `funds_unavailable`, `not_ready`) may be
//! retried inside the freshness window; anything carrying a transaction digest
//! must be resolved before another payout is requested.

use anyhow::{anyhow, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::Deserialize;

pub const TESTNET_FAUCET: &str = "https://faucet.testnet.sui.io";
const DOMAIN: &str = "sui-faucet-pow/1";
const SALT: &[u8] = b"sui-faucet-pow-1";
/// Spec section 3: the only accepted proof-of-work version.
const VERSION: u32 = 1;

/// A `GET /v3/challenge` response. Parameter fields are validated on parse —
/// a challenge that disagrees with our implemented spec version is rejected
/// before any grinding (spec section 7).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub version: u32,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub salt: String,
    #[serde(default)]
    pub algorithm: String,
    #[serde(default)]
    pub argon2_version: u32,
    #[serde(default)]
    pub memory_size: u32,
    #[serde(default)]
    pub iterations: u32,
    #[serde(default)]
    pub parallelism: u32,
    #[serde(default)]
    pub hash_length: usize,
    pub network: String,
    pub chain_id: String,
    pub checkpoint_seq: String,
    pub checkpoint_digest: String,
    pub random_bytes: String,
    pub faucet_address: String,
    pub recipient: String,
    pub difficulty: String,
    pub threshold: String,
    #[serde(default)]
    pub window_seconds: u64,
    #[serde(default)]
    pub amount_mist: String,
}

impl Challenge {
    /// Rejects challenges whose version or hash parameters differ from the
    /// implemented spec (section 7: unrecognized versions must be rejected).
    fn validate(&self) -> Result<()> {
        if self.version != VERSION
            || self.domain != DOMAIN
            || self.salt.as_bytes() != SALT
            || self.algorithm != "argon2d"
            || self.argon2_version != 19
            || self.memory_size != 8192
            || self.iterations != 1
            || self.parallelism != 1
            || self.hash_length != 32
        {
            return Err(anyhow!(
                "faucet challenge disagrees with the implemented PoW v1 spec"
            ));
        }
        if self.network != "testnet" {
            return Err(anyhow!(
                "faucet challenge is for {}, not testnet",
                self.network
            ));
        }
        Ok(())
    }

    fn threshold_u64(&self) -> Result<u64> {
        self.threshold
            .parse::<u64>()
            .context("faucet threshold outside the u64 range")
    }
}

fn normalize_address(addr: &str) -> Result<String> {
    let hex = addr
        .strip_prefix("0x")
        .ok_or_else(|| anyhow!("address missing 0x prefix: {addr}"))?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("address is not hex: {addr}"));
    }
    Ok(format!("0x{:0>64}", hex.to_ascii_lowercase()))
}

/// The eight-field preimage, newline-joined per spec section 2. No trailing
/// newline; integers in canonical decimal form; addresses zero-extended.
pub fn preimage(
    chain_id: &str,
    checkpoint_seq: u64,
    checkpoint_digest: &str,
    random_b64: &str,
    faucet_address: &str,
    recipient: &str,
    nonce: u64,
) -> String {
    format!(
        "{DOMAIN}\n{chain_id}\n{checkpoint_seq}\n{checkpoint_digest}\n{random_b64}\n{}\n{}\n{nonce}",
        normalize_address(faucet_address)
            .expect("faucet address normalization"),
        normalize_address(recipient).expect("recipient normalization"),
    )
}

/// Argon2d per spec section 3: v=19, m=8192 KiB, t=1, p=1, 32 output bytes,
/// salt `sui-faucet-pow-1`.
pub fn argon2d_hash(preimage: &str) -> Result<[u8; 32]> {
    let params = Params::new(8192, 1, 1, Some(32)).context("PoW params are statically valid")?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2d, Version::V0x13, params)
        .hash_password_into(preimage.as_bytes(), SALT, &mut out)
        .map_err(|e| anyhow!("argon2d: {e}"))?;
    Ok(out)
}

/// First 8 output bytes, big-endian (spec section 4).
pub fn leading_u64(hash: &[u8; 32]) -> u64 {
    u64::from_be_bytes(hash[..8].try_into().expect("32 >= 8"))
}

/// `floor(2^64 / difficulty)` via 128-bit math — the numerator does not fit
/// in 64 bits (spec section 4).
pub fn threshold_for(difficulty: u64) -> u64 {
    (((u128::from(u64::MAX)) + 1) / u128::from(difficulty)) as u64
}

/// Grinds nonces from zero until the leading u64 clears the challenge's
/// threshold divided by `margin` — the spec's overshoot rule: a proof that
/// clears `margin`-times the required difficulty earns a proportionally
/// longer freshness window, which is what makes a slow (debug-profile)
/// grinder viable. `margin` 1 grinds exactly the quoted difficulty.
/// Difficulty ~3e3 costs a few thousand Argon2d hashes (~30 s in release).
pub fn solve(
    challenge: &Challenge,
    deadline: std::time::Instant,
    margin: u64,
) -> Result<(u64, String)> {
    let threshold = challenge.threshold_u64()?;
    let target = (threshold / margin.max(1)).max(1);
    let mut nonce: u64 = 0;
    loop {
        let pre = preimage(
            &challenge.chain_id,
            challenge.checkpoint_seq.parse::<u64>()?,
            &challenge.checkpoint_digest,
            &challenge.random_bytes,
            &challenge.faucet_address,
            &challenge.recipient,
            nonce,
        );
        let hash = argon2d_hash(&pre)?;
        if leading_u64(&hash) < target {
            return Ok((nonce, hex::encode(hash)));
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "faucet PoW grind exceeded its deadline at nonce {nonce}"
            ));
        }
        nonce += 1;
    }
}

/// The four submitted fields of `POST /v3/gas`, all wire-format strings.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proof {
    pub recipient: String,
    pub checkpoint_seq: String,
    pub nonce: String,
    pub hash_hex: String,
}

/// Outcome of `POST /v3/gas`, mapped from the spec's error-code table
/// (section 6). Retryable variants may be resubmitted inside the freshness
/// window; digest-carrying variants must be resolved before another payout.
#[derive(Debug, Clone)]
pub enum GasOutcome {
    Paid { digest: String, amount_mist: u64 },
    Retryable { code: String },
    AlreadyUsed { digest: Option<String> },
    PayoutUnknown { digest: String },
    Rejected { code: String },
}

/// Mints test SUI into `recipient`'s address balance: challenge → grind →
/// submit, with the spec's retry semantics (bounded attempts, backoff).
pub async fn request_gas(
    faucet: &str,
    recipient: &str,
    http: &reqwest::Client,
    attempts: usize,
    margin: u64,
) -> Result<GasOutcome> {
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(15 * attempt as u64)).await;
        }
        let ch = fetch_challenge(faucet, recipient, http).await?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let ch2 = ch.clone();
        let (nonce, hash_hex) = tokio::task::spawn_blocking(move || solve(&ch2, deadline, margin))
            .await
            .map_err(|e| anyhow!("poW grind task panicked: {e}"))??;
        let proof = Proof {
            recipient: normalize_address(recipient)?,
            checkpoint_seq: ch.checkpoint_seq.clone(),
            nonce: nonce.to_string(),
            hash_hex,
        };
        let resp = http
            .post(format!("{faucet}/v3/gas"))
            .json(&proof)
            .send()
            .await
            .context("POST /v3/gas")?;
        let status = resp.status();
        let body_text = resp.text().await.context("reading /v3/gas body")?;
        eprintln!(
            "faucet: POST payload {}",
            serde_json::to_string(&proof).unwrap_or_default()
        );
        eprintln!("faucet: response {status} {body_text}");
        let body: serde_json::Value =
            serde_json::from_str(&body_text).context("parsing /v3/gas body")?;
        let code = body
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        let digest =
            |b: &serde_json::Value| b.get("digest").and_then(|d| d.as_str()).map(str::to_string);
        if status.is_success() {
            return Ok(GasOutcome::Paid {
                digest: digest(&body).unwrap_or_default(),
                amount_mist: body
                    .get("amountMist")
                    .and_then(|a| a.as_str())
                    .and_then(|a| a.parse().ok())
                    .unwrap_or(0),
            });
        }
        match code.as_str() {
            "stale_checkpoint" | "overloaded" | "funds_unavailable" | "not_ready" => {
                eprintln!("faucet: retryable {code} (attempt {})", attempt + 1);
                continue;
            }
            "already_used" => {
                return Ok(GasOutcome::AlreadyUsed {
                    digest: digest(&body),
                })
            }
            "payout_status_unknown" => {
                let d = digest(&body).unwrap_or_default();
                return Ok(GasOutcome::PayoutUnknown { digest: d });
            }
            other => return Err(anyhow!("faucet rejected the payout: {other}")),
        }
    }
    Err(anyhow!("faucet did not pay after {attempts} attempts"))
}

/// Fetches and validates a challenge for `recipient`.
pub async fn fetch_challenge(
    faucet: &str,
    recipient: &str,
    http: &reqwest::Client,
) -> Result<Challenge> {
    let ch: Challenge = http
        .get(format!("{faucet}/v3/challenge"))
        .query(&[("recipient", recipient)])
        .send()
        .await
        .context("GET /v3/challenge")?
        .json()
        .await
        .context("parsing challenge")?;
    ch.validate().context("challenge validation")?;
    Ok(ch)
}

/// Tops the address balance up past `min_mist` from the faucet, verifying the
/// balance actually moved. Returns the final balance.
pub async fn ensure_balance(
    http: &reqwest::Client,
    faucet: &str,
    rpc_url: &str,
    recipient: &str,
    min_mist: u64,
    attempts: usize,
    margin: u64,
) -> Result<u64> {
    let balance = || async {
        podseq_sui::settlement::sui_balance(rpc_url, recipient)
            .await
            .context("checking SUI balance")
    };
    if balance().await? >= min_mist {
        return balance().await;
    }
    eprintln!("faucet: balance under {min_mist} MIST; running the PoW refill");
    match request_gas(faucet, recipient, http, attempts, margin).await? {
        GasOutcome::Paid { amount_mist, .. } => {
            eprintln!("faucet: paid {amount_mist} MIST");
        }
        GasOutcome::AlreadyUsed { digest } => {
            eprintln!(
                "faucet: proof already used (digest {digest:?}); continuing on the balance check"
            );
        }
        GasOutcome::PayoutUnknown { digest } => {
            return Err(anyhow!(
                "faucet payout status unknown (digest {digest}); resolve it before refilling"
            ));
        }
        GasOutcome::Retryable { code } | GasOutcome::Rejected { code } => {
            return Err(anyhow!("faucet refill failed: {code}"));
        }
    }
    let final_balance = balance().await?;
    if final_balance < min_mist {
        return Err(anyhow!(
            "faucet refill ran but the balance is still {final_balance} < {min_mist} MIST"
        ));
    }
    Ok(final_balance)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("fixtures/pow-vectors.json");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Vectors {
        domain: String,
        salt: String,
        params: Params_,
        inputs: Inputs,
        cases: Vec<Case>,
        thresholds: std::collections::BTreeMap<String, String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params_ {
        argon2_version: u32,
        #[serde(rename = "memoryKiB")]
        memory_kib: u32,
        iterations: u32,
        parallelism: u32,
        output_bytes: usize,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Inputs {
        chain_id: String,
        checkpoint_seq: String,
        checkpoint_digest: String,
        random_bytes: String,
        #[serde(rename = "faucetAddress")]
        faucet_address: String,
        recipient: String,
    }
    #[derive(Deserialize)]
    struct Case {
        note: String,
        #[serde(rename = "randomBytes")]
        random_bytes: String,
        nonce: String,
        preimage: String,
        hash: String,
        v: String,
    }

    fn vectors() -> Vectors {
        serde_json::from_str(VECTORS).expect("conformance vectors parse")
    }

    /// Spec section 8 conformance: preimage join, normalization, Argon2d
    /// output, and the leading-u64 rule against the official vectors.
    #[test]
    fn conformance_vectors_hold() {
        let v = vectors();
        assert_eq!(v.domain, DOMAIN);
        assert_eq!(v.salt.as_bytes(), SALT);
        assert_eq!(v.params.argon2_version, 19);
        assert_eq!(v.params.memory_kib, 8192);
        assert_eq!(v.params.iterations, 1);
        assert_eq!(v.params.parallelism, 1);
        assert_eq!(v.params.output_bytes, 32);
        for case in &v.cases {
            let pre = preimage(
                &v.inputs.chain_id,
                v.inputs.checkpoint_seq.parse().unwrap(),
                &v.inputs.checkpoint_digest,
                &case.random_bytes,
                &v.inputs.faucet_address,
                &v.inputs.recipient,
                case.nonce.parse().expect("nonce is canonical decimal"),
            );
            assert_eq!(pre, case.preimage, "preimage join: {}", case.note);
            let hash = argon2d_hash(&pre).expect("argon2d");
            assert_eq!(hex::encode(hash), case.hash, "hash: {}", case.note);
            let v: u64 = case.v.parse().expect("v is canonical decimal");
            assert_eq!(leading_u64(&hash), v, "leading u64: {}", case.note);
        }
    }

    /// Spec section 4: `floor(2^64 / difficulty)` via 128-bit math, checked
    /// against the official threshold table (including values above 2^63).
    #[test]
    fn threshold_matches_the_official_table() {
        let v = vectors();
        for (difficulty, expected) in &v.thresholds {
            if difficulty == "$comment" {
                continue;
            }
            let d: u64 = difficulty.parse().expect("difficulty is a decimal integer");
            assert_eq!(
                threshold_for(d).to_string(),
                *expected,
                "threshold at difficulty {d}"
            );
        }
    }

    /// Normalization: short-form addresses zero-extend to 64 lowercase hex.
    #[test]
    fn addresses_normalize_to_canonical_form() {
        assert_eq!(
            normalize_address("0x2").unwrap(),
            "0x0000000000000000000000000000000000000000000000000000000000000002"
        );
        assert_eq!(
            normalize_address("0xABCDEF0000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
            "0xabcdef0000000000000000000000000000000000000000000000000000000001"
        );
        assert!(normalize_address("1234").is_err());
    }

    /// Solving a synthetic easy challenge finds a nonce whose leading u64
    /// clears the threshold and reproduces the submitted hash.
    #[test]
    fn solve_clears_the_threshold() {
        let challenge = Challenge {
            version: 1,
            domain: DOMAIN.into(),
            salt: SALT.iter().map(|b| *b as char).collect(),
            algorithm: "argon2d".into(),
            argon2_version: 19,
            memory_size: 8192,
            iterations: 1,
            parallelism: 1,
            hash_length: 32,
            network: "testnet".into(),
            chain_id: "8wWZfv1HjQxjB5ncmC9CQqqRqUMBiEZXnc5FhS9zRe1v".into(),
            checkpoint_seq: "1713342".into(),
            checkpoint_digest: "GzBdc9tbUe3s2PK1iVJcyQBj5TvdX7bJiAHWbhBDYiYb".into(),
            random_bytes: "mJ4ZqDXQasy17GTiGfp0/OYAGCR+7reLM5+Gb39YtoAIaPuh1wj+z5J7po+eSze3".into(),
            faucet_address: "0x949cd75a2485cc18b8258634ffd9b849a687e2e2a0b4c2444a21f007fc79adc4"
                .into(),
            recipient: "0x1111111111111111111111111111111111111111111111111111111111111111".into(),
            difficulty: "2".into(),
            threshold: "9223372036854775808".into(),
            window_seconds: 3600,
            amount_mist: "1000000000".into(),
        };
        let (nonce, hash_hex) = solve(
            &challenge,
            std::time::Instant::now() + std::time::Duration::from_secs(120),
            1,
        )
        .expect("solve");
        let hash = argon2d_hash(&preimage(
            &challenge.chain_id,
            1713342,
            &challenge.checkpoint_digest,
            &challenge.random_bytes,
            &challenge.faucet_address,
            &challenge.recipient,
            nonce,
        ))
        .unwrap();
        assert_eq!(hex::encode(hash), hash_hex);
        assert!(leading_u64(&hash) < 9223372036854775808);
    }
}

/// Live round-trip against the public testnet faucet (ignored by default:
/// `cargo test -p podseq-e2e --lib faucet::tests::live -- --ignored`).
/// Pays a fixed dev address; asserts a payout or a proof already spent.
#[cfg(test)]
mod live {
    use super::*;

    const DEV_RECIPIENT: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";

    #[tokio::test]
    #[ignore]
    async fn live_roundtrip_pays_the_recipient() {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap();
        match request_gas(TESTNET_FAUCET, DEV_RECIPIENT, &http, 3, 8)
            .await
            .unwrap()
        {
            GasOutcome::Paid {
                digest,
                amount_mist,
            } => {
                println!("live: paid {amount_mist} MIST, digest {digest}");
                assert!(amount_mist >= 1_000_000_000);
            }
            GasOutcome::AlreadyUsed { digest } => {
                println!("live: proof already spent, digest {digest:?} — wire path verified");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}
