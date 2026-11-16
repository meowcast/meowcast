//! Developer access. SPAKE2 authenticates both SSH keys and derives a separate cloud
//! admission credential. No password verifier crosses the network. All access leases
//! are boot-local; enrollment is a separate, two-minute action on the TV.
use crate::{cloud, state::{self, Shared}, util};
use base64::{Engine, engine::general_purpose::STANDARD};
use curve25519_dalek::edwards::CompressedEdwardsY;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::{path::Path, process::Command};

pub const DIR: &str = "/data/net/debug";
pub const CGROUP: &str = "/sys/fs/cgroup/meow-debug";
pub const LEASE_S: f64 = 3600.0;
pub const PAIR_S: f64 = 120.0;
pub const MAX_DEVELOPERS: usize = 8;
const CLIENT: &[u8] = b"meowdbg-v2/client";
const SERVER: &[u8] = b"meowdbg-v2/dongle";

#[derive(Clone, Serialize, Deserialize)]
pub struct Developer {
    pub id: String,
    pub key: String,
    pub token_hash: String,
}

pub struct Pending {
    id: String,
    key: String,
    secret: Vec<u8>,
    payload: String,
    until: f64,
    generation: u64,
}

pub fn ssh_key(key: &str) -> Option<String> {
    let mut words = key.split_whitespace();
    let (kind, encoded) = (words.next()?, words.next()?);
    let blob = STANDARD.decode(encoded).ok()?;
    // SSH wire encoding: string("ssh-ed25519"), string(32-byte key).
    if kind != "ssh-ed25519" || blob.len() != 51 || &blob[..19] != b"\0\0\0\x0bssh-ed25519\0\0\0\x20" {
        return None;
    }
    let label: String = words.next().unwrap_or("computer").chars().filter(|c| c.is_ascii_alphanumeric() || "-_.@".contains(*c)).take(48).collect();
    Some(format!("ssh-ed25519 {} {}", STANDARD.encode(blob), label))
}

fn write_developers(dir: &Path, developers: &[Developer]) -> Result<(), &'static str> {
    // The canonical revocation survives power loss before authorized_keys is rebuilt.
    let body = serde_json::to_vec(developers).map_err(|_| "storage")?;
    util::write_atomic(&dir.join("developers-v2.json"), &body, 0o600).map_err(|_| "storage")?;
    write_ssh_keys(dir, developers)
}

fn write_ssh_keys(dir: &Path, developers: &[Developer]) -> Result<(), &'static str> {
    let keys = developers.iter().map(|d| format!("{}\n", d.key)).collect::<String>();
    util::write_atomic(&dir.join("authorized_keys"), keys.as_bytes(), 0o600).map_err(|_| "storage")
}

pub fn prepare(shared: &Shared) -> Result<(), &'static str> {
    write_ssh_keys(Path::new(DIR), &shared.0.lock().unwrap().developers)?;
    host_key().map(|_| ())
}

pub fn init(shared: &Shared) {
    util::mkdir_private(DIR);
    let developers: Vec<Developer> = std::fs::read(format!("{DIR}/developers-v2.json")).ok()
        .and_then(|s| serde_json::from_slice(&s).ok()).unwrap_or_default();
    let developers: Vec<_> = developers.into_iter().filter(|d| ssh_key(&d.key).is_some() && d.token_hash.len() == 64
        && d.token_hash.bytes().all(|b| b.is_ascii_hexdigit()) && d.id == util::sha256_hex(d.key.split(' ').take(2).collect::<Vec<_>>().join(" ").as_bytes())[..16]).take(MAX_DEVELOPERS).collect();
    // Legacy enrollment cannot establish a pinned host key or a cloud credential.
    // Existing users must pair once with v2; preserve the device's host key.
    let _ = std::fs::remove_file(format!("{DIR}/enabled"));
    if write_developers(Path::new(DIR), &developers).is_ok() {
        shared.0.lock().unwrap().developers = developers;
    }
}

pub fn control(shared: &Shared, cmd: &str) {
    let mut n = shared.0.lock().unwrap();
    match cmd {
        "debug on" | "debug off" => {
            n.debug = cmd == "debug on";
            n.debug_until = if n.debug { util::uptime() + LEASE_S } else { 0.0 };
            n.debug_lan = false;
            n.debug_cloud = false;
            n.debug_pair_until = 0.0;
            n.debug_pending = None;
            n.debug_code.clear();
        }
        "debug pair" if n.debug && n.debug_running && util::uptime() >= n.debug_locked_until => {
            n.debug_code = util::random_digits(6);
            n.debug_pair_until = util::uptime() + PAIR_S;
            n.debug_pending = None;
            // Do not reset the guess budget when reopening the window.
        }
        "debug lan on" | "debug lan off" if n.debug => n.debug_lan = cmd.ends_with(" on"),
        "debug cloud on" | "debug cloud off" if n.debug => n.debug_cloud = cmd.ends_with(" on"),
        _ if cmd.starts_with("debug revoke ") => {
            let id = cmd.trim_start_matches("debug revoke ");
            let remaining: Vec<_> = n.developers.iter().filter(|d| id != "all" && d.id != id).cloned().collect();
            // Fail closed on storage errors and end current sessions as well.
            n.developers = remaining;
            if write_developers(Path::new(DIR), &n.developers).is_err() {
                n.debug = false;
                util::log("netd: could not persist developer revocation; debugging disabled");
            }
        }
        _ => return,
    }
    n.debug_generation += 1;
    drop(n);
    state::changed(shared);
}

pub fn expire(shared: &Shared) {
    let mut n = shared.0.lock().unwrap();
    let now = util::uptime();
    let mut changed = false;
    if n.debug && now >= n.debug_until {
        n.debug = false;
        n.debug_lan = false;
        n.debug_cloud = false;
        n.debug_generation += 1;
        changed = true;
    }
    if (!n.debug || now >= n.debug_pair_until) && (!n.debug_code.is_empty() || n.debug_pending.is_some()) {
        n.debug_code.clear();
        n.debug_pending = None;
        n.debug_pair_until = 0.0;
        changed = true;
    }
    drop(n);
    if changed { state::changed(shared); }
}

pub fn host_key() -> Result<String, &'static str> {
    let path = format!("{DIR}/ed25519_host_key");
    if !Path::new(&path).exists() {
        let status = Command::new("dropbearkey").args(["-t", "ed25519", "-f", &path]).output().map_err(|_| "host_key")?;
        if !status.status.success() { return Err("host_key"); }
    }
    let output = Command::new("dropbearkey").args(["-y", "-f", &path]).output().map_err(|_| "host_key")?;
    String::from_utf8_lossy(&output.stdout).lines().find_map(ssh_key).ok_or("host_key")
}

fn proof(secret: &[u8], role: &str, payload: &str) -> String {
    util::hex(&util::hmac_sha256(secret, format!("meowdbg-v2/{role}\n{payload}").as_bytes()))
}

fn start(code: &str, key: &str, message: &[u8], host: &str, device: &str, now: f64, generation: u64) -> Result<(Pending, Value), &'static str> {
    let key = ssh_key(key).ok_or("bad_key")?;
    if message.len() != 33 || message[0] != b'A' { return Err("bad_message"); }
    // Match python-spake2's strict prime-order group decoder. The Rust library
    // only decompresses Edwards points, so enforce subgroup and canonical encoding
    // here before feeding an untrusted point into PAKE.
    let encoded: [u8; 32] = message[1..].try_into().map_err(|_| "bad_message")?;
    let point = CompressedEdwardsY(encoded).decompress().ok_or("bad_message")?;
    if point.is_small_order() || !point.is_torsion_free() || point.compress().to_bytes() != encoded { return Err("bad_message"); }
    let (state, reply) = Spake2::<Ed25519Group>::start_b(&Password::new(code.as_bytes()), &Identity::new(CLIENT), &Identity::new(SERVER));
    let secret = state.finish(message).map_err(|_| "bad_message")?;
    let id = util::random_token();
    // A single canonical string is authenticated byte-for-byte by both peers.
    let payload = json!({"device": device, "host_key": host, "key": key, "session": id, "version": 2}).to_string();
    let server_proof = proof(&secret, "server", &payload);
    let response = json!({"ok": true, "session": id, "message": util::hex(&reply), "payload": payload, "proof": server_proof});
    Ok((Pending { id, key, secret, payload, until: now + 30.0, generation }, response))
}

pub fn pair_start(shared: &Shared, b: &Value) -> Result<Value, &'static str> {
    let message = decode_hex(b["message"].as_str().unwrap_or(""), 33).ok_or("bad_message")?;
    let mut n = shared.0.lock().unwrap();
    let now = util::uptime();
    if !n.debug || !n.debug_running || now >= n.debug_until || now >= n.debug_pair_until { return Err("not_open"); }
    if now < n.debug_locked_until { return Err("locked"); }
    if n.debug_pending.as_ref().is_some_and(|p| now < p.until) { return Err("busy"); }
    let key = ssh_key(b["key"].as_str().unwrap_or("")).ok_or("bad_key")?;
    let id = util::sha256_hex(key.split(' ').take(2).collect::<Vec<_>>().join(" ").as_bytes())[..16].to_string();
    if n.developers.len() >= MAX_DEVELOPERS && !n.developers.iter().any(|d| d.id == id) { return Err("key_limit"); }
    // Every start permits one online guess, including abandoned exchanges.
    n.debug_fails += 1;
    if n.debug_fails > 5 {
        n.debug_locked_until = now + 30.0;
        n.debug_pair_until = 0.0;
        n.debug_code.clear();
        drop(n);
        state::changed(shared);
        return Err("locked");
    }
    let host = host_key()?;
    let (device, _) = cloud::identity().ok_or("identity")?;
    let (pending, response) = start(&n.debug_code, b["key"].as_str().unwrap_or(""), &message, &host, &device, now, n.debug_generation)?;
    n.debug_pending = Some(pending);
    Ok(response)
}

pub fn pair_finish(shared: &Shared, b: &Value) -> Result<Value, &'static str> {
    let mut n = shared.0.lock().unwrap();
    // An unrelated request must not consume the legitimate in-progress exchange.
    if n.debug_pending.as_ref().is_none_or(|p| Some(p.id.as_str()) != b["session"].as_str()) { return Err("bad_session"); }
    let p = n.debug_pending.take().ok_or("bad_session")?;
    if !n.debug || !n.debug_running || util::uptime() >= n.debug_until || util::uptime() >= n.debug_pair_until
        || util::uptime() >= p.until || p.generation != n.debug_generation { return Err("not_open"); }
    if !util::ct_eq(proof(&p.secret, "client", &p.payload).as_bytes(), b["proof"].as_str().unwrap_or("").as_bytes()) { return Err("bad_code"); }
    let token = proof(&p.secret, "admission", &p.payload);
    let id = util::sha256_hex(p.key.split(' ').take(2).collect::<Vec<_>>().join(" ").as_bytes())[..16].to_string();
    let mut developers = n.developers.clone();
    developers.retain(|d| d.id != id);
    developers.push(Developer { id, key: p.key, token_hash: util::sha256_hex(token.as_bytes()) });
    write_developers(Path::new(DIR), &developers)?;
    n.developers = developers;
    n.debug_code.clear();
    n.debug_pair_until = 0.0;
    n.debug_fails = 0;
    n.debug_generation += 1;
    let response = json!({"ok": true, "proof": proof(&p.secret, "complete", &p.payload)});
    drop(n);
    state::changed(shared);
    Ok(response)
}

pub fn decode_hex(s: &str, bytes: usize) -> Option<Vec<u8>> {
    if s.len() != bytes * 2 { return None; }
    s.as_bytes().chunks_exact(2).map(|p| Some(((p[0] as char).to_digit(16)? * 16 + (p[1] as char).to_digit(16)?) as u8)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> String { format!("ssh-ed25519 {} test", STANDARD.encode([b"\0\0\0\x0bssh-ed25519\0\0\0\x20".as_slice(), &[7u8;32]].concat())) }
    #[test]
    fn pake_binds_keys_and_confirms_both_directions() {
        let (client, message) = Spake2::<Ed25519Group>::start_a(&Password::new(b"123456"), &Identity::new(CLIENT), &Identity::new(SERVER));
        let (p, response) = start("123456", &key(), &message, &key(), "device", 0.0, 7).unwrap();
        let secret = client.finish(&decode_hex(response["message"].as_str().unwrap(), 33).unwrap()).unwrap();
        assert_eq!(secret, p.secret);
        assert_eq!(proof(&secret, "server", &p.payload), response["proof"]);
        assert_ne!(proof(&secret, "client", &p.payload), response["proof"]);
        assert_ne!(proof(&secret, "server", &p.payload.replace("device", "impostor")), response["proof"]);
    }
    #[test]
    fn wrong_password_cannot_confirm() {
        let (client, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(b"000000"), &Identity::new(CLIENT), &Identity::new(SERVER));
        let (p, r) = start("123456", &key(), &msg, &key(), "device", 0.0, 0).unwrap();
        let secret = client.finish(&decode_hex(r["message"].as_str().unwrap(), 33).unwrap()).unwrap();
        assert_ne!(proof(&secret, "client", &p.payload), proof(&p.secret, "client", &p.payload));
    }
    #[test]
    fn rejects_identity_and_small_subgroup_messages() {
        let mut message = vec![b'A'];
        message.extend_from_slice(&[0u8; 32]);
        assert!(start("123456", &key(), &message, &key(), "device", 0.0, 0).is_err());
        message[1] = 1;
        assert!(start("123456", &key(), &message, &key(), "device", 0.0, 0).is_err());
    }
    #[test]
    fn rejects_malformed_ssh_keys() {
        assert!(ssh_key(&key()).is_some());
        assert!(ssh_key("ssh-ed25519 AAAA attacker").is_none());
        assert!(ssh_key(&key().replace("ssh-ed25519", "ssh-rsa")).is_none());
    }
    #[test]
    fn off_and_expiry_invalidate_pending_pairing() {
        let shared = state::new();
        control(&shared, "debug on");
        {
            let mut n = shared.0.lock().unwrap();
            n.debug_until = util::uptime() - 1.0;
            n.debug_cloud = true;
            n.debug_lan = true;
            n.debug_pair_until = util::uptime() + 120.0;
            n.debug_code = "123456".into();
        }
        expire(&shared);
        let n = shared.0.lock().unwrap();
        assert!(!n.debug && !n.debug_lan && !n.debug_cloud);
        assert!(n.debug_code.is_empty() && n.debug_pending.is_none());
    }
    #[test]
    fn failed_confirmation_consumes_session_and_replay_is_rejected() {
        let shared = state::new();
        let (_, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(b"123456"), &Identity::new(CLIENT), &Identity::new(SERVER));
        let now = util::uptime();
        let (p, _) = start("123456", &key(), &msg, &key(), "device", now, 0).unwrap();
        let request = json!({"session":p.id,"proof":"wrong proof"});
        {
            let mut n = shared.0.lock().unwrap();
            n.debug = true;
            n.debug_running = true;
            n.debug_until = now + 100.0;
            n.debug_pair_until = now + 60.0;
            n.debug_pending = Some(p);
        }
        assert_eq!(pair_finish(&shared, &request).unwrap_err(), "bad_code");
        assert_eq!(pair_finish(&shared, &request).unwrap_err(), "bad_session");
    }
    #[test]
    fn abandoned_exchanges_exhaust_the_guess_budget() {
        let shared = state::new();
        {
            let mut n = shared.0.lock().unwrap();
            n.debug = true;
            n.debug_running = true;
            n.debug_until = util::uptime() + 100.0;
            n.debug_pair_until = util::uptime() + 60.0;
            n.debug_code = "123456".into();
            n.debug_fails = 5;
        }
        let (_, message) = Spake2::<Ed25519Group>::start_a(&Password::new(b"123456"), &Identity::new(CLIENT), &Identity::new(SERVER));
        assert_eq!(pair_start(&shared, &json!({"key":key(),"message":util::hex(&message)})).unwrap_err(), "locked");
        control(&shared, "debug pair");
        let n = shared.0.lock().unwrap();
        assert!(n.debug_locked_until > util::uptime());
        assert!(n.debug_code.is_empty());
        assert_eq!(n.debug_pair_until, 0.0);
    }
    #[test]
    fn canonical_store_and_ssh_keys_rebuild_without_revoked_keys() {
        let dir = std::env::temp_dir().join(format!("meow-debug-test-{}", util::random_token()));
        std::fs::create_dir(&dir).unwrap();
        let dev = Developer { id:"id".into(), key:key(), token_hash:"a".repeat(64) };
        write_developers(&dir, &[dev]).unwrap();
        assert!(std::fs::read_to_string(dir.join("authorized_keys")).unwrap().contains("ssh-ed25519"));
        write_developers(&dir, &[]).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("developers-v2.json")).unwrap(), "[]");
        assert_eq!(std::fs::read_to_string(dir.join("authorized_keys")).unwrap(), "");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[ignore = "requires Python with tools/requirements.txt; set MEOWDBG_PYTHON"]
    fn python_cli_interoperability() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::Stdio;
        let python = std::env::var("MEOWDBG_PYTHON").expect("MEOWDBG_PYTHON");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/test_meowdbg.py");
        let mut child = Command::new(python).args([fixture.to_str().unwrap(), "--interop"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        let msg = decode_hex(req["body"]["message"].as_str().unwrap(), 33).unwrap();
        let (p, response) = start("123456", req["body"]["key"].as_str().unwrap(), &msg, &key(), &"a".repeat(32), 0.0, 0).unwrap();
        writeln!(input, "{response}").unwrap(); input.flush().unwrap();
        line.clear(); output.read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["step"], "finish");
        assert_eq!(req["body"]["proof"], proof(&p.secret, "client", &p.payload));
        writeln!(input, "{}", json!({"ok":true,"proof":proof(&p.secret,"complete",&p.payload)})).unwrap(); input.flush().unwrap();
        line.clear(); output.read_line(&mut line).unwrap();
        let profile: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(profile["token"], proof(&p.secret, "admission", &p.payload));
        assert!(child.wait().unwrap().success());
    }
}
