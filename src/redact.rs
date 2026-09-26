// Outbound secret masking. What the gateway sends a vendor has its secrets
// (API keys, private keys, tokens, passwords), and when asked personal data
// and the user's own words, swapped for placeholders like
// {{API_KEY_k3v9x2mq}}; what the vendor says back has them swapped back,
// streams included. A placeholder is the same for the same value every time
// (a keyed hash of it), so cached turns and thinking signatures stay good,
// and one placeholder is never two values. The values are held in memory
// only; a restart loses nothing, since the next request brings them again.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{LazyLock, Mutex},
};

use sha2::{Digest, Sha256};

const MAX_VALUES: usize = 200_000;

const ALNUM: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const TOKEN_CH: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
const EMAIL_BOUND: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._%+-";

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub secrets: bool,
    pub personal: bool,
    pub words: Vec<String>,
}

static VALUES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static KEY_OF: LazyLock<Mutex<Option<Vec<u8>>>> = LazyLock::new(|| Mutex::new(None));
static KEY_PATH: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

// KeyPath is where the key placeholders are made with is kept, so they stay
// the same across restarts. Set before the first mask; None keeps the key in
// memory only.
pub fn set_key_path(path: impl Into<String>) {
    *KEY_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path.into());
}

// Known says there are values to put back.
pub fn known() -> bool {
    !VALUES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty()
}

fn key() -> Vec<u8> {
    let mut slot = KEY_OF
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(k) = slot.as_ref() {
        return k.clone();
    }
    let k = load_key();
    *slot = Some(k.clone());
    k
}

fn load_key() -> Vec<u8> {
    let path = KEY_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(path) = &path {
        if let Ok(b) = fs::read(path) {
            if b.len() >= 32 {
                return b[..32].to_vec();
            }
        }
    }
    let mut b = vec![0u8; 32];
    let _ = getrandom::fill(&mut b);
    if let Some(path) = &path {
        if let Some(dir) = Path::new(path).parent() {
            let _ = fs::create_dir_all(dir);
        }
        #[cfg(unix)]
        {
            use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};
            if let Ok(mut f) = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
            {
                let _ = f.write_all(&b);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = fs::write(path, &b);
        }
    }
    b
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut pad = [0u8; BLOCK];
    if key.len() > BLOCK {
        pad[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        pad[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for (i, k) in pad.iter().enumerate() {
        ipad[i] ^= k;
        opad[i] ^= k;
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner.finalize());
    let out = outer.finalize();
    let mut h = [0u8; 32];
    h.copy_from_slice(&out);
    h
}

fn b32_tag(h: &[u8]) -> String {
    const ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let bits = (u64::from(h[0]) << 32)
        | (u64::from(h[1]) << 24)
        | (u64::from(h[2]) << 16)
        | (u64::from(h[3]) << 8)
        | u64::from(h[4]);
    (0..8)
        .map(|i| ALPHA[((bits >> (35 - i * 5)) & 0x1f) as usize] as char)
        .collect::<String>()
        .to_ascii_lowercase()
}

// ---- masking --------------------------------------------------------------------

pub fn mask(s: &str, o: &Options) -> (String, usize) {
    if s.len() < 3 {
        return (s.to_owned(), 0);
    }
    let mut found: Vec<(usize, usize, &'static str)> = Vec::new();
    let b = s.as_bytes();
    for w in &o.words {
        let w = w.trim();
        if w.len() < 2 {
            continue;
        }
        let mut i = 0;
        while let Some(rel) = find_from(b, i, w.as_bytes()) {
            found.push((rel, rel + w.len(), "TERM"));
            i = rel + w.len();
        }
    }
    for r in RULES {
        let want = if r.personal { o.personal } else { o.secrets };
        if !want {
            continue;
        }
        for (a, e) in (r.find)(s) {
            if !r.bound.is_empty() && bounded(s, a, e, r.bound) {
                continue;
            }
            if let Some(ok) = r.ok {
                if !ok(&s[a..e]) {
                    continue;
                }
            }
            found.push((a, e, r.kind));
        }
    }
    if found.is_empty() {
        return (s.to_owned(), 0);
    }
    // nothing inside a placeholder already there
    if s.contains("{{") {
        for (pa, pe) in find_placeholders(s) {
            found.retain(|f| !(f.0 < pe && f.1 > pa));
        }
        if found.is_empty() {
            return (s.to_owned(), 0);
        }
    }
    // the first to start, and of those the longest, wins where they overlap
    found.sort_by(|x, y| x.0.cmp(&y.0).then(y.1.cmp(&x.1)));
    let mut out = String::with_capacity(s.len());
    let (mut last, mut n) = (0, 0);
    for (a, e, kind) in found {
        if a < last {
            continue;
        }
        out.push_str(&s[last..a]);
        out.push_str(&placeholder(kind, &s[a..e]));
        last = e;
        n += 1;
    }
    out.push_str(&s[last..]);
    (out, n)
}

struct Rule {
    kind: &'static str,
    personal: bool,
    bound: &'static str,
    ok: Option<fn(&str) -> bool>,
    find: fn(&str) -> Vec<(usize, usize)>,
}

static RULES: &[Rule] = &[
    Rule {
        kind: "PRIVATE_KEY",
        personal: false,
        bound: "",
        ok: None,
        find: find_private_key,
    },
    Rule {
        kind: "API_KEY",
        personal: false,
        bound: TOKEN_CH,
        ok: None,
        find: find_sk,
    },
    Rule {
        kind: "API_KEY",
        personal: false,
        bound: TOKEN_CH,
        ok: None,
        find: find_gh,
    },
    Rule {
        kind: "API_KEY",
        personal: false,
        bound: TOKEN_CH,
        ok: None,
        find: find_google,
    },
    Rule {
        kind: "API_KEY",
        personal: false,
        bound: TOKEN_CH,
        ok: None,
        find: find_slack,
    },
    Rule {
        kind: "API_KEY",
        personal: false,
        bound: TOKEN_CH,
        ok: None,
        find: find_stripe,
    },
    Rule {
        kind: "API_KEY",
        personal: false,
        bound: ALNUM,
        ok: None,
        find: find_aws,
    },
    Rule {
        kind: "API_KEY",
        personal: false,
        bound: TOKEN_CH,
        ok: None,
        find: find_misc,
    },
    Rule {
        kind: "TOKEN",
        personal: false,
        bound: TOKEN_CH,
        ok: None,
        find: find_jwt,
    },
    Rule {
        kind: "PASSWORD",
        personal: false,
        bound: "",
        ok: Some(not_a_variable),
        find: find_password,
    },
    Rule {
        kind: "SECRET",
        personal: false,
        bound: "",
        ok: Some(secret_value),
        find: find_secret,
    },
    Rule {
        kind: "EMAIL",
        personal: true,
        bound: EMAIL_BOUND,
        ok: Some(real_email),
        find: find_email,
    },
    Rule {
        kind: "ID_CARD",
        personal: true,
        bound: ALNUM,
        ok: Some(chinese_id),
        find: find_id_card,
    },
    Rule {
        kind: "PHONE",
        personal: true,
        bound: "0123456789",
        ok: None,
        find: find_phone,
    },
    Rule {
        kind: "BANK_CARD",
        personal: true,
        bound: "0123456789",
        ok: Some(luhn),
        find: find_bank_card,
    },
];

fn find_from(h: &[u8], from: usize, n: &[u8]) -> Option<usize> {
    if from > h.len() {
        return None;
    }
    find_sub(&h[from..], n).map(|p| p + from)
}

fn find_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w == n)
}

fn run(b: &[u8], mut i: usize, pred: impl Fn(u8) -> bool) -> usize {
    while i < b.len() && pred(b[i]) {
        i += 1;
    }
    i
}

fn bounded(s: &str, a: usize, e: usize, bound: &str) -> bool {
    let b = s.as_bytes();
    let bb = bound.as_bytes();
    (a > 0 && bb.contains(&b[a - 1])) || (e < b.len() && bb.contains(&b[e]))
}

fn is_token_ch(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn is_word_ch(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-'
}

fn is_secret_value_ch(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'_' | b'-'
                | b'.'
                | b'/'
                | b'+'
                | b'='
                | b'~'
                | b'!'
                | b'@'
                | b'#'
                | b'%'
                | b'^'
                | b'&'
                | b'*'
        )
}

// -----BEGIN (words )PRIVATE KEY( BLOCK)?----- ... -----END ...-----
fn find_private_key(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find_from(b, i, b"-----BEGIN ") {
        let Some(q) = key_words(b, p + 11) else {
            i = p + 1;
            continue;
        };
        let mut end = None;
        let mut e = q + 1;
        while let Some(c) = find_from(b, e, b"-----END ") {
            if let Some(f) = key_words(b, c + 9) {
                end = Some(f);
                break;
            }
            e = c + 1;
        }
        let Some(f) = end else {
            i = p + 1;
            continue;
        };
        out.push((p, f));
        i = f;
    }
    out
}

// after "-----BEGIN " / "-----END ": ([A-Z0-9]+ )*PRIVATE KEY( BLOCK)?-----
fn key_words(b: &[u8], start: usize) -> Option<usize> {
    let mut bounds = vec![start];
    let mut i = start;
    loop {
        let w = i;
        while i < b.len() && (b[i].is_ascii_uppercase() || b[i].is_ascii_digit()) {
            i += 1;
        }
        if i > w && i < b.len() && b[i] == b' ' {
            i += 1;
            bounds.push(i);
        } else {
            break;
        }
    }
    for &p in bounds.iter().rev() {
        if !b[p..].starts_with(b"PRIVATE KEY") {
            continue;
        }
        let mut e = p + 11;
        if b[e..].starts_with(b" BLOCK") {
            e += 6;
        }
        if b[e..].starts_with(b"-----") {
            return Some(e + 5);
        }
    }
    None
}

// sk-(ant-|proj-|or-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}
fn find_sk(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find_from(b, i, b"sk-") {
        let q = p + 3;
        let mut end = None;
        for pfx in ["ant-", "proj-", "or-", "svcacct-", "admin-"] {
            if b[q..].starts_with(pfx.as_bytes()) {
                let e = run(b, q + pfx.len(), is_token_ch);
                if e - q - pfx.len() >= 20 {
                    end = Some(e);
                    break;
                }
            }
        }
        let e = end.unwrap_or_else(|| run(b, q, is_token_ch));
        if e - q >= 20 {
            out.push((p, e));
            i = e;
        } else {
            i = p + 1;
        }
    }
    out
}

// gh[pousr]_[A-Za-z0-9]{30,} | github_pat_[A-Za-z0-9_]{40,} | glpat-[A-Za-z0-9_-]{20,}
fn find_gh(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let mut next = i + 1;
        if b[i] == b'g' {
            if let Some(e) = gh_at(b, i) {
                out.push((i, e));
                next = e;
            }
        }
        i = next;
    }
    out
}

fn gh_at(b: &[u8], i: usize) -> Option<usize> {
    if b.len() > i + 3 && matches!(b[i + 2], b'p' | b'o' | b'u' | b's' | b'r') && b[i + 3] == b'_' {
        let e = run(b, i + 4, |c| c.is_ascii_alphanumeric());
        if e - (i + 4) >= 30 {
            return Some(e);
        }
    }
    if b[i..].starts_with(b"github_pat_") {
        let e = run(b, i + 11, |c| c.is_ascii_alphanumeric() || c == b'_');
        if e - (i + 11) >= 40 {
            return Some(e);
        }
    }
    if b[i..].starts_with(b"glpat-") {
        let e = run(b, i + 6, is_token_ch);
        if e - (i + 6) >= 20 {
            return Some(e);
        }
    }
    None
}

// AIza[0-9A-Za-z_-]{35}
fn find_google(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find_from(b, i, b"AIza") {
        if run(b, p + 4, is_token_ch) - (p + 4) >= 35 {
            out.push((p, p + 39));
            i = p + 39;
        } else {
            i = p + 1;
        }
    }
    out
}

// xox[abposr]-[0-9A-Za-z-]{10,}
fn find_slack(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find_from(b, i, b"xox") {
        let q = p + 3;
        if q + 1 < b.len()
            && matches!(b[q], b'a' | b'b' | b'p' | b'o' | b's' | b'r')
            && b[q + 1] == b'-'
        {
            let e = run(b, q + 2, |c| c.is_ascii_alphanumeric() || c == b'-');
            if e - (q + 2) >= 10 {
                out.push((p, e));
                i = e;
                continue;
            }
        }
        i = p + 1;
    }
    out
}

// (sk|rk)_live_[0-9A-Za-z]{20,}
fn find_stripe(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find_from(b, i, b"_live_") {
        if p >= 2 && (&b[p - 2..p] == b"sk" || &b[p - 2..p] == b"rk") {
            let e = run(b, p + 6, |c| c.is_ascii_alphanumeric());
            if e - (p + 6) >= 20 {
                out.push((p - 2, e));
                i = e;
                continue;
            }
        }
        i = p + 1;
    }
    out
}

// (AKIA|ASIA)[0-9A-Z]{16}
fn find_aws(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let mut next = i + 1;
        if b[i] == b'A' && (b[i..].starts_with(b"AKIA") || b[i..].starts_with(b"ASIA")) {
            let e = run(b, i + 4, |c| c.is_ascii_uppercase() || c.is_ascii_digit());
            if e - (i + 4) >= 16 {
                out.push((i, i + 20));
                next = i + 20;
            }
        }
        i = next;
    }
    out
}

// hf_ | gsk_ | xai- | npm_ | pypi- | dop_v1_ | SG.
fn find_misc(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let mut next = i + 1;
        if let Some(e) = misc_at(b, i) {
            out.push((i, e));
            next = e;
        }
        i = next;
    }
    out
}

fn misc_at(b: &[u8], i: usize) -> Option<usize> {
    if b[i..].starts_with(b"hf_") {
        let e = run(b, i + 3, |c| c.is_ascii_alphanumeric());
        return (e - (i + 3) >= 30).then_some(e);
    }
    if b[i..].starts_with(b"gsk_") {
        let e = run(b, i + 4, |c| c.is_ascii_alphanumeric());
        return (e - (i + 4) >= 40).then_some(e);
    }
    if b[i..].starts_with(b"xai-") {
        let e = run(b, i + 4, |c| c.is_ascii_alphanumeric());
        return (e - (i + 4) >= 40).then_some(e);
    }
    if b[i..].starts_with(b"npm_") {
        let e = run(b, i + 4, |c| c.is_ascii_alphanumeric());
        return (e - (i + 4) >= 36).then_some(i + 40);
    }
    if b[i..].starts_with(b"pypi-") {
        let e = run(b, i + 5, is_token_ch);
        return (e - (i + 5) >= 50).then_some(e);
    }
    if b[i..].starts_with(b"dop_v1_") {
        let e = run(b, i + 7, |c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'));
        return (e - (i + 7) >= 64).then_some(i + 71);
    }
    if b[i..].starts_with(b"SG.") {
        let e = run(b, i + 3, is_token_ch);
        if e - (i + 3) >= 22
            && b.get(i + 25) == Some(&b'.')
            && run(b, i + 26, is_token_ch) - (i + 26) >= 43
        {
            return Some(i + 69);
        }
    }
    None
}

// eyJ[A-Za-z0-9_-]{8,}.eyJ[A-Za-z0-9_-]{8,}.[A-Za-z0-9_-]{8,}
fn find_jwt(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find_from(b, i, b"eyJ") {
        let e1 = run(b, p + 3, is_token_ch);
        let ok = e1 - (p + 3) >= 8
            && b.get(e1) == Some(&b'.')
            && b.len() >= e1 + 4
            && &b[e1 + 1..e1 + 4] == b"eyJ";
        if !ok {
            i = p + 1;
            continue;
        }
        let e2 = run(b, e1 + 4, is_token_ch);
        if e2 - (e1 + 4) < 8 || b.get(e2) != Some(&b'.') {
            i = p + 1;
            continue;
        }
        let e3 = run(b, e2 + 1, is_token_ch);
        if e3 - (e2 + 1) < 8 {
            i = p + 1;
            continue;
        }
        out.push((p, e3));
        i = e3;
    }
    out
}

// scheme://user:(password)@ with the password taken
fn find_password(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let user_ch = |c: u8| {
        !c.is_ascii_whitespace()
            && !matches!(
                c,
                b':' | b'@' | b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>'
            )
    };
    let pass_ch = |c: u8| {
        !c.is_ascii_whitespace()
            && !matches!(c, b'@' | b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>')
    };
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find_from(b, i, b"://") {
        i = p + 3;
        let mut r0 = p;
        while r0 > 0
            && (b[r0 - 1].is_ascii_alphanumeric() || matches!(b[r0 - 1], b'+' | b'.' | b'-'))
        {
            r0 -= 1;
        }
        let Some(start) = (r0..p).find(|&k| b[k].is_ascii_alphabetic()) else {
            continue;
        };
        let u0 = p + 3;
        let u1 = run(b, u0, user_ch);
        if u1 == u0 || b.get(u1) != Some(&b':') {
            continue;
        }
        let w0 = u1 + 1;
        let w1 = run(b, w0, pass_ch);
        if w1 - w0 < 3 || b.get(w1) != Some(&b'@') {
            continue;
        }
        out.push((w0, w1));
        i = w1 + 1;
    }
    out
}

// name = value as .env files and configs have them, with the value taken
const SECRET_MARKERS: &[&str] = &[
    "pass",
    "PASS",
    "Pass",
    "secret",
    "SECRET",
    "Secret",
    "token",
    "TOKEN",
    "Token",
    "key",
    "KEY",
    "Key",
    "credential",
    "CREDENTIAL",
    "Credential",
];

fn find_secret(s: &str) -> Vec<(usize, usize)> {
    if !SECRET_MARKERS.iter().any(|m| s.contains(m)) {
        return vec![];
    }
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if !is_word_ch(b[i]) {
            i += 1;
            continue;
        }
        let r = run(b, i, is_word_ch);
        match secret_at(b, i, r) {
            Some((w0, w1)) => {
                out.push((w0, w1));
                i = w1;
            }
            None => i = r,
        }
    }
    out
}

// (password|passwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credential)
fn keyword_at(b: &[u8], i: usize) -> Option<usize> {
    let rest = &b[i..];
    for lit in ["password", "passwd", "secret", "token", "credential"] {
        let lb = lit.as_bytes();
        if rest.len() >= lb.len() && rest[..lb.len()].eq_ignore_ascii_case(lb) {
            return Some(i + lb.len());
        }
    }
    for pre in ["api", "access", "private"] {
        let pb = pre.as_bytes();
        if rest.len() < pb.len() + 3 || !rest[..pb.len()].eq_ignore_ascii_case(pb) {
            continue;
        }
        let mut j = pb.len();
        if matches!(rest.get(j), Some(b'_') | Some(b'-')) {
            j += 1;
        }
        if rest.len() >= j + 3 && rest[j..j + 3].eq_ignore_ascii_case(b"key") {
            return Some(i + j + 3);
        }
    }
    None
}

fn secret_at(b: &[u8], i: usize, r: usize) -> Option<(usize, usize)> {
    for pe in (i..=r).rev() {
        let Some(q) = keyword_at(b, pe) else {
            continue;
        };
        let qe = run(b, q, is_word_ch);
        let mut t = qe;
        if matches!(b.get(t), Some(b'"') | Some(b'\'')) {
            t += 1;
        }
        while matches!(b.get(t), Some(b' ') | Some(b'\t')) {
            t += 1;
        }
        if !matches!(b.get(t), Some(b':') | Some(b'=')) {
            continue;
        }
        t += 1;
        while matches!(b.get(t), Some(b' ') | Some(b'\t')) {
            t += 1;
        }
        if matches!(b.get(t), Some(b'"') | Some(b'\'')) {
            t += 1;
        }
        let v1 = run(b, t, is_secret_value_ch);
        if v1 - t >= 8 {
            return Some((t, v1));
        }
    }
    None
}

// [A-Za-z0-9._%+-]+@domain with the whole address taken
fn find_email(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let local_ch =
        |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'%' | b'+' | b'-');
    let mut out = vec![];
    let mut i = 0;
    while let Some(at) = find_from(b, i, b"@") {
        i = at + 1;
        let mut l0 = at;
        while l0 > 0 && local_ch(b[l0 - 1]) {
            l0 -= 1;
        }
        if l0 == at {
            continue;
        }
        let Some(end) = domain_end(b, at + 1) else {
            continue;
        };
        out.push((l0, end));
        i = end;
    }
    out
}

// [A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,} from p, greedy with backtracking
fn domain_end(b: &[u8], p: usize) -> Option<usize> {
    let l1 = run(b, p, |c| c.is_ascii_alphanumeric() || c == b'-');
    if l1 == p {
        return None;
    }
    let mut p = l1;
    let mut segs: Vec<(usize, usize, usize)> = vec![]; // (dot, seg start, seg end)
    while b.get(p) == Some(&b'.') {
        let s0 = p + 1;
        let s1 = run(b, s0, |c| c.is_ascii_alphanumeric() || c == b'-');
        if s1 == s0 {
            break;
        }
        segs.push((p, s0, s1));
        p = s1;
    }
    for (_, s0, s1) in segs.iter().rev() {
        let mut e = *s0;
        while e < *s1 && b[e].is_ascii_alphabetic() {
            e += 1;
        }
        if e - s0 >= 2 {
            return Some(e);
        }
    }
    None
}

// [1-9]\d{5}(18|19|20)\d{2}(0[1-9]|1[0-2])(0[1-9]|[12]\d|3[01])\d{3}[\dXx]
fn find_id_card(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i + 18 <= b.len() {
        let ok = (b'1'..=b'9').contains(&b[i])
            && b[i..i + 6].iter().all(u8::is_ascii_digit)
            && matches!(&b[i + 6..i + 8], b"18" | b"19" | b"20")
            && b[i + 8].is_ascii_digit()
            && b[i + 9].is_ascii_digit()
            && month_ok(b[i + 10], b[i + 11])
            && day_ok(b[i + 12], b[i + 13])
            && b[i + 14].is_ascii_digit()
            && b[i + 15].is_ascii_digit()
            && b[i + 16].is_ascii_digit()
            && (b[i + 17].is_ascii_digit() || b[i + 17] == b'X' || b[i + 17] == b'x');
        if ok {
            out.push((i, i + 18));
            i += 18;
        } else {
            i += 1;
        }
    }
    out
}

fn month_ok(m0: u8, m1: u8) -> bool {
    (m0 == b'0' && (b'1'..=b'9').contains(&m1)) || (m0 == b'1' && matches!(m1, b'0' | b'1' | b'2'))
}

fn day_ok(d0: u8, d1: u8) -> bool {
    (d0 == b'0' && (b'1'..=b'9').contains(&d1))
        || (matches!(d0, b'1' | b'2') && d1.is_ascii_digit())
        || (d0 == b'3' && matches!(d1, b'0' | b'1'))
}

// (\+?86[- ]?)?1[3-9]\d{9}
fn find_phone(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let mut hit = None;
        let mut j = i;
        if b[j] == b'+' {
            j += 1;
        }
        if j + 1 < b.len() && b[j] == b'8' && b[j + 1] == b'6' {
            let k0 = j + 2;
            if matches!(b.get(k0), Some(b'-') | Some(b' ')) {
                if let Some(e) = phone_rest(b, k0 + 1) {
                    hit = Some((i, e));
                }
            }
            if hit.is_none() {
                if let Some(e) = phone_rest(b, k0) {
                    hit = Some((i, e));
                }
            }
        }
        if hit.is_none() {
            if let Some(e) = phone_rest(b, i) {
                hit = Some((i, e));
            }
        }
        match hit {
            Some((a, e)) => {
                out.push((a, e));
                i = e;
            }
            None => i += 1,
        }
    }
    out
}

fn phone_rest(b: &[u8], i: usize) -> Option<usize> {
    if i + 11 > b.len() || b[i] != b'1' || !(b'3'..=b'9').contains(&b[i + 1]) {
        return None;
    }
    if !b[i + 2..i + 11].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(i + 11)
}

// [3-6]\d{3}([ -]?\d{4}){2,3}([ -]?\d{1,3})?
fn find_bank_card(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if (b'3'..=b'6').contains(&b[i]) {
            if let Some(e) = bank_at(b, i) {
                out.push((i, e));
                i = e;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn bank_at(b: &[u8], i: usize) -> Option<usize> {
    if i + 4 > b.len() || !b[i..i + 4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut p = i + 4;
    let mut groups = 0;
    while groups < 3 {
        let Some(next) = bank_group(b, p) else {
            break;
        };
        p = next;
        groups += 1;
    }
    if groups < 2 {
        return None;
    }
    let mut q = p;
    if matches!(b.get(q), Some(b' ') | Some(b'-')) {
        q += 1;
    }
    let mut e = q;
    let mut n = 0;
    while e < b.len() && n < 3 && b[e].is_ascii_digit() {
        e += 1;
        n += 1;
    }
    if n >= 1 {
        p = e;
    }
    Some(p)
}

fn bank_group(b: &[u8], p: usize) -> Option<usize> {
    if matches!(b.get(p), Some(b' ') | Some(b'-')) {
        if p + 5 <= b.len() && b[p + 1..p + 5].iter().all(u8::is_ascii_digit) {
            return Some(p + 5);
        }
    }
    (p + 4 <= b.len() && b[p..p + 4].iter().all(u8::is_ascii_digit)).then_some(p + 4)
}

// notAVariable turns away what stands in for a value: ${PASS}, <password>, ****.
fn not_a_variable(v: &str) -> bool {
    let b = v.as_bytes();
    if b.is_empty() || matches!(b[0], b'$' | b'%' | b'{' | b'<' | b'[' | b'*') {
        return false;
    }
    !v.trim_matches(|c| matches!(c, '*' | 'x' | 'X' | '.'))
        .is_empty()
}

// a value that looks like a secret, not code: letters and digits both
fn secret_value(v: &str) -> bool {
    if !not_a_variable(v) || v.starts_with("process.env") || v.starts_with("os.") {
        return false;
    }
    v.bytes().any(|c| c.is_ascii_digit()) && v.bytes().any(|c| c.is_ascii_alphabetic())
}

fn real_email(v: &str) -> bool {
    let host = v[v.rfind('@').unwrap_or(0) + 1..].to_ascii_lowercase();
    for fake in [
        "example.com",
        "example.org",
        "example.net",
        "localhost",
        ".test",
        ".invalid",
        ".example",
    ] {
        if host == fake.trim_start_matches('.') || host.ends_with(fake) {
            return false;
        }
    }
    !host.starts_with("noreply") && !v.contains("noreply@")
}

fn chinese_id(v: &str) -> bool {
    const W: [i32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECK: &[u8] = b"10X98765432";
    let b = v.as_bytes();
    let sum: i32 = (0..17).map(|i| i32::from(b[i] - b'0') * W[i]).sum();
    b[17].to_ascii_uppercase() == CHECK[(sum % 11) as usize]
}

fn luhn(v: &str) -> bool {
    let ds: Vec<u32> = v
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|c| u32::from(c - b'0'))
        .collect();
    if ds.len() < 15 || ds.len() > 19 {
        return false;
    }
    let mut sum = 0;
    for (i, d) in ds.iter().rev().enumerate() {
        let mut d = *d;
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    sum % 10 == 0
}

// ---- placeholders ---------------------------------------------------------------

fn placeholder(kind: &str, v: &str) -> String {
    let mut msg = Vec::with_capacity(kind.len() + v.len() + 1);
    msg.extend_from_slice(kind.as_bytes());
    msg.push(0);
    msg.extend_from_slice(v.as_bytes());
    let h = hmac_sha256(&key(), &msg);
    let mut p = String::with_capacity(kind.len() + 14);
    p.push_str("{{");
    p.push_str(kind);
    p.push('_');
    p.push_str(&b32_tag(&h[..5]));
    p.push_str("}}");
    let mut values = VALUES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !values.contains_key(&p) {
        if values.len() >= MAX_VALUES {
            let drop: Vec<String> = values.keys().take(MAX_VALUES / 2).cloned().collect();
            for k in drop {
                values.remove(k.as_str());
            }
        }
        values.insert(p.clone(), v.to_owned());
    }
    p
}

// a placeholder as mask writes it: {{[A-Z][A-Z_]*_[a-z2-7]{8}}}
fn find_placeholders(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(rel) = find_from(b, i, b"{{") {
        let r0 = rel + 2;
        let mut e = r0;
        while e < b.len() && (b[e].is_ascii_uppercase() || b[e] == b'_') {
            e += 1;
        }
        let body_ok = e + 10 <= b.len()
            && e - r0 >= 2
            && b[r0].is_ascii_uppercase()
            && b[e - 1] == b'_'
            && b[e..e + 8]
                .iter()
                .all(|c| (b'a'..=b'z').contains(c) || (b'2'..=b'7').contains(c))
            && &b[e + 8..e + 10] == b"}}";
        if body_ok {
            out.push((rel, e + 10));
            i = e + 10;
        } else {
            i = rel + 1;
        }
    }
    out
}

// ---- restoring ------------------------------------------------------------------

// Restore puts the values back in s. escaped is for text that is itself JSON
// (a tool call's arguments), where a value goes in escaped.
pub fn restore(s: &str, escaped: bool) -> String {
    if !s.contains("{{") {
        return s.to_owned();
    }
    let values = VALUES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, e) in find_placeholders(s) {
        out.push_str(&s[last..a]);
        match values.get(&s[a..e]) {
            Some(v) if escaped => out.push_str(&json_escape(v)),
            Some(v) => out.push_str(v),
            None => out.push_str(&s[a..e]),
        }
        last = e;
    }
    out.push_str(&s[last..]);
    out
}

// how much of the end of s may be the start of a placeholder the next piece
// of a stream finishes
fn partial_tail(s: &str) -> usize {
    let b = s.as_bytes();
    let Some(mut i) = b.iter().rposition(|&c| c == b'{') else {
        return 0;
    };
    if i > 0 && b[i - 1] == b'{' {
        i -= 1;
    }
    let t = &b[i..];
    if t.len() > 32 || find_sub(t, b"}}").is_some() {
        return 0;
    }
    if t == b"{" || t == b"{{" {
        return t.len();
    }
    if !t.starts_with(b"{{") {
        return 0;
    }
    let body = if t.ends_with(b"}") {
        &t[2..t.len() - 1]
    } else {
        &t[2..]
    };
    if body.is_empty() || !body[0].is_ascii_uppercase() {
        return 0;
    }
    if body.iter().all(|&c| {
        c.is_ascii_uppercase()
            || c == b'_'
            || (b'a'..=b'z').contains(&c)
            || (b'2'..=b'7').contains(&c)
    }) {
        return t.len();
    }
    0
}

// ---- JSON -----------------------------------------------------------------------

// keys whose strings are left alone: ids, names and kinds the vendor needs as
// they are, and what is sealed or encoded (a thinking block's signature,
// reasoning's encrypted content, an image's data)
fn keep(key: &str, s: &str) -> bool {
    matches!(
        key,
        "signature"
            | "encrypted_content"
            | "data"
            | "thoughtSignature"
            | "thought_signature"
            | "model"
            | "id"
            | "tool_use_id"
            | "call_id"
            | "item_id"
            | "type"
            | "role"
            | "name"
            | "previous_response_id"
            | "prompt_cache_key"
            | "media_type"
            | "mime_type"
            | "mimeType"
            | "url"
            | "image_url"
            | "file_id"
            | "reasoning_effort"
            | "effort"
            | "stop_reason"
            | "finish_reason"
            | "status"
            | "object"
            | "event"
    ) || s.starts_with("data:")
}

// a string under key holds JSON text of its own, so a value put back in it
// goes in escaped: a tool call's arguments, as a whole or streamed in pieces
fn as_json(key: &str, event_type: &str) -> bool {
    key == "arguments"
        || key == "partial_json"
        || key == "delta" && event_type.contains("arguments")
}

// calls f with every string value in the JSON document b — its path (keys and
// indexes joined by dots) and the key it is under — and writes what f returns
// in its place. Everything else is kept byte for byte; None if not JSON.
fn walk<F>(b: &[u8], f: F) -> Option<Vec<u8>>
where
    F: FnMut(&str, &str, &str) -> String,
{
    let mut w = Walker {
        b,
        out: Vec::new(),
        last: 0,
        f,
    };
    let i = w.value(w.ws(0), "", "")?;
    if w.ws(i) != b.len() {
        return None;
    }
    if w.last == 0 {
        return Some(b.to_vec());
    }
    w.out.extend_from_slice(&b[w.last..]);
    Some(w.out)
}

struct Walker<'a, F: FnMut(&str, &str, &str) -> String> {
    b: &'a [u8],
    out: Vec<u8>,
    last: usize,
    f: F,
}

impl<F: FnMut(&str, &str, &str) -> String> Walker<'_, F> {
    fn ws(&self, mut i: usize) -> usize {
        while i < self.b.len() && matches!(self.b[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        i
    }

    fn value(&mut self, i: usize, path: &str, key: &str) -> Option<usize> {
        let b = self.b;
        if i >= b.len() {
            return None;
        }
        match b[i] {
            b'{' => {
                let mut i = self.ws(i + 1);
                if i < b.len() && b[i] == b'}' {
                    return Some(i + 1);
                }
                loop {
                    if i >= b.len() || b[i] != b'"' {
                        return None;
                    }
                    let (end, esc) = str_end(b, i)?;
                    let k = decode_str(b, i, end, esc)?;
                    i = self.ws(end);
                    if i >= b.len() || b[i] != b':' {
                        return None;
                    }
                    let child = join(path, &k);
                    i = self.value(self.ws(i + 1), &child, &k)?;
                    i = self.ws(i);
                    if i < b.len() && b[i] == b',' {
                        i = self.ws(i + 1);
                        continue;
                    }
                    if i < b.len() && b[i] == b'}' {
                        return Some(i + 1);
                    }
                    return None;
                }
            }
            b'[' => {
                let mut i = self.ws(i + 1);
                if i < b.len() && b[i] == b']' {
                    return Some(i + 1);
                }
                let mut n = 0;
                loop {
                    let child = join(path, &n.to_string());
                    // an array's strings are under the key the array is
                    i = self.value(i, &child, key)?;
                    i = self.ws(i);
                    if i < b.len() && b[i] == b',' {
                        i = self.ws(i + 1);
                        n += 1;
                        continue;
                    }
                    if i < b.len() && b[i] == b']' {
                        return Some(i + 1);
                    }
                    return None;
                }
            }
            b'"' => {
                let (end, esc) = str_end(b, i)?;
                let s = decode_str(b, i, end, esc)?;
                let t = (self.f)(path, key, &s);
                if t != s {
                    self.out.extend_from_slice(&b[self.last..i]);
                    self.out.push(b'"');
                    self.out.extend_from_slice(json_escape(&t).as_bytes());
                    self.out.push(b'"');
                    self.last = end;
                }
                Some(end)
            }
            _ => {
                let mut j = i;
                while j < b.len()
                    && !matches!(b[j], b',' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n')
                {
                    j += 1;
                }
                (j > i).then_some(j)
            }
        }
    }
}

// the end of the string literal that starts at i, and whether it has escapes
fn str_end(b: &[u8], i: usize) -> Option<(usize, bool)> {
    let mut escaped = false;
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => {
                escaped = true;
                j += 2;
            }
            b'"' => return Some((j + 1, escaped)),
            _ => j += 1,
        }
    }
    None
}

fn decode_str(b: &[u8], i: usize, end: usize, escaped: bool) -> Option<String> {
    if escaped {
        serde_json::from_slice::<String>(&b[i..end]).ok()
    } else {
        Some(String::from_utf8_lossy(&b[i + 1..end - 1]).into_owned())
    }
}

fn join(path: &str, k: &str) -> String {
    if path.is_empty() {
        k.to_owned()
    } else {
        format!("{path}.{k}")
    }
}

// s as it goes between the quotes of a JSON string
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// masks the strings of a request body, and says how many values it masked.
// A body that isn't JSON is masked as text.
pub fn mask_json(body: &[u8], o: &Options) -> (Vec<u8>, usize) {
    let mut total = 0;
    match walk(body, |_, key, s| {
        if keep(key, s) {
            return s.to_owned();
        }
        let (t, n) = mask(s, o);
        total += n;
        t
    }) {
        Some(out) => (out, total),
        None => {
            let (t, n) = mask(&String::from_utf8_lossy(body), o);
            (t.into_bytes(), n)
        }
    }
}

// puts the values back in a response body
pub fn restore_json(body: &[u8]) -> Vec<u8> {
    if find_sub(body, b"{{").is_none() {
        return body.to_vec();
    }
    let mut typ = String::new();
    match walk(body, |path, key, s| {
        if path == "type" {
            typ = s.to_owned();
        }
        restore(s, as_json(key, &typ))
    }) {
        Some(out) => out,
        None => restore(&String::from_utf8_lossy(body), false).into_bytes(),
    }
}

// ---- response writer ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unknown,
    Stream,
    Whole,
    AsIs,
}

// held is an event that ended in what may be the start of a placeholder
struct Held {
    ctx: String,
    path: String,
    tail: String,
    full: Vec<u8>,
    cut: Vec<u8>,
}

// Writer puts the values back in what a vendor answers, on its way to the
// agent. Build it with the response headers (None when the header is
// absent); a stream is put back event by event, a whole body at finish, and
// a compressed body is left as it is. Each write returns the bytes to
// forward; finish returns what is still held and must be called last.
//
// A placeholder a stream splits between two events — {{API_ in one text
// delta, KEY_k3v9x2mq}} in the next — is put back whole: an event whose text
// ends in what may be the start of one is held until the next, and if that
// one goes on with the same text the start moves over to it. Nothing is
// added or dropped, so the text the agent puts together is the same.
pub struct Writer {
    mode: Mode,
    buf: Vec<u8>,
    pending: Option<Held>,
    done: bool,
}

impl Writer {
    pub fn new(content_type: Option<&str>, content_encoding: Option<&str>) -> Self {
        let mode = if content_encoding
            .is_some_and(|e| !e.is_empty() && !e.eq_ignore_ascii_case("identity"))
        {
            Mode::AsIs
        } else if content_type.is_some_and(|ct| ct.contains("event-stream")) {
            Mode::Stream
        } else if content_type.is_some_and(|ct| !ct.is_empty()) {
            Mode::Whole
        } else {
            Mode::Unknown
        };
        Writer {
            mode,
            buf: Vec::new(),
            pending: None,
            done: false,
        }
    }

    pub fn write(&mut self, p: &[u8]) -> Vec<u8> {
        if self.mode == Mode::Unknown {
            // the ChatGPT backend streams without saying so
            self.mode =
                if p.starts_with(b"event:") || p.starts_with(b"data:") || p.starts_with(b":") {
                    Mode::Stream
                } else {
                    Mode::Whole
                };
        }
        match self.mode {
            Mode::AsIs => return p.to_vec(),
            Mode::Whole => {
                self.buf.extend_from_slice(p);
                return Vec::new();
            }
            _ => {}
        }
        self.buf.extend_from_slice(p);
        let mut out = Vec::new();
        while let Some((i, n)) = event_end(&self.buf) {
            let ev = self.buf[..i + n].to_vec();
            self.buf.drain(..i + n);
            self.event(&ev, &mut out);
        }
        out
    }

    // Finish writes what is still held.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.done = true;
        match self.mode {
            Mode::Whole => out.extend_from_slice(&restore_json(&self.buf)),
            Mode::Stream => {
                if let Some(held) = self.pending.take() {
                    out.extend_from_slice(&held.full);
                }
                if !self.buf.is_empty() {
                    out.extend_from_slice(
                        restore(&String::from_utf8_lossy(&self.buf), false).as_bytes(),
                    );
                }
            }
            _ => {}
        }
        out
    }

    // event puts the values back in one event and writes it, or holds it
    fn event(&mut self, ev: &[u8], out: &mut Vec<u8>) {
        let Some((name, data, pre, post)) = split_event(ev) else {
            self.release(out);
            out.extend_from_slice(restore(&String::from_utf8_lossy(ev), false).as_bytes());
            return;
        };
        let ctx = format!("{name}|{}", event_ctx(data));
        let mut carry = String::new();
        let mut at = String::new();
        if let Some(held) = self.pending.take() {
            if held.ctx == ctx && has_path(data, &held.path) {
                out.extend_from_slice(&held.cut);
                carry = held.tail;
                at = held.path;
            } else {
                out.extend_from_slice(&held.full);
            }
        }
        let mut typ = String::new();
        let mut tail_path = String::new();
        let mut tail = String::new();
        let restored = walk(data, |path, key, s| {
            if path == "type" {
                typ = s.to_owned();
            }
            let owned;
            let mut s = s;
            if path == at.as_str() {
                owned = format!("{carry}{s}");
                s = owned.as_str();
            }
            if keep(key, s) {
                return s.to_owned();
            }
            let s = restore(s, as_json(key, &typ));
            let n = partial_tail(&s);
            if n > 0 {
                tail_path = path.to_owned();
                tail = s[s.len() - n..].to_owned();
            }
            s
        })
        .unwrap_or_else(|| data.to_vec());
        let full = rebuild(pre, &restored, post);
        if tail.is_empty() {
            out.extend_from_slice(&full);
            return;
        }
        let cut_data = walk(&restored, |path, _, s| {
            if path == tail_path.as_str() {
                s.strip_suffix(tail.as_str()).unwrap_or(s).to_owned()
            } else {
                s.to_owned()
            }
        })
        .unwrap_or_else(|| restored.clone());
        self.pending = Some(Held {
            ctx,
            path: tail_path,
            tail,
            full,
            cut: rebuild(pre, &cut_data, post),
        });
    }

    fn release(&mut self, out: &mut Vec<u8>) {
        if let Some(held) = self.pending.take() {
            out.extend_from_slice(&held.full);
        }
    }
}

// where the first complete event in b ends, and the length of the blank line
// that ends it
fn event_end(b: &[u8]) -> Option<(usize, usize)> {
    let i = find_sub(b, b"\n\n");
    let j = find_sub(b, b"\r\n\r\n");
    match (i, j) {
        (None, None) => None,
        (Some(i), Some(j)) if j < i => Some((j, 4)),
        (Some(i), _) => Some((i, 2)),
        (None, Some(j)) => Some((j, 4)),
    }
}

fn rebuild(pre: &[u8], data: &[u8], post: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pre.len() + data.len() + post.len());
    out.extend_from_slice(pre);
    out.extend_from_slice(data);
    out.extend_from_slice(post);
    out
}

// finds the one data line of an event that has JSON in it: what comes before
// the JSON, the JSON, and what comes after it
fn split_event(ev: &[u8]) -> Option<(String, &[u8], &[u8], &[u8])> {
    let lines: Vec<&[u8]> = ev.split_inclusive(|&c| c == b'\n').collect();
    let mut name = String::new();
    let mut at = None;
    let mut off = 0;
    for (idx, line) in lines.iter().enumerate() {
        let t = trim_right(line);
        if t.starts_with(b"event:") {
            name = String::from_utf8_lossy(&t[6..]).trim().to_owned();
        } else if t.starts_with(b"data:") {
            if at.is_some() {
                return None; // data over more than one line
            }
            at = Some(idx);
        }
        if at.is_none() {
            off += line.len();
        }
    }
    let ai = at?;
    let t = trim_right(lines[ai]);
    let start = if t.len() > 5 && t[5] == b' ' { 6 } else { 5 };
    let d = &t[start..];
    if d.is_empty() || (d[0] != b'{' && d[0] != b'[') {
        return None;
    }
    Some((name, d, &ev[..off + start], &ev[off + t.len()..]))
}

fn trim_right(mut b: &[u8]) -> &[u8] {
    while let Some(&c) = b.last() {
        if c == b'\r' || c == b'\n' {
            b = &b[..b.len() - 1];
        } else {
            break;
        }
    }
    b
}

// what says which text an event goes on with
fn event_ctx(data: &[u8]) -> String {
    let mut b = String::new();
    walk(data, |path, _, s| {
        if path == "type" || path == "item_id" {
            b.push_str(path);
            b.push('=');
            b.push_str(s);
            b.push(';');
        }
        s.to_owned()
    });
    for k in ["\"index\":", "\"output_index\":", "\"content_index\":"] {
        if let Some(i) = find_sub(data, k.as_bytes()) {
            let j = i + k.len();
            let mut e = j;
            while e < data.len() && (data[e].is_ascii_digit() || data[e] == b' ') {
                e += 1;
            }
            b.push_str(k);
            b.push_str(&String::from_utf8_lossy(&data[j..e]));
            b.push(';');
        }
    }
    b
}

fn has_path(data: &[u8], path: &str) -> bool {
    let mut found = false;
    walk(data, |p, _, s| {
        if p == path {
            found = true;
        }
        s.to_owned()
    });
    found
}

// ---- tests ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OPENAI_KEY: &str = "sk-proj-abcdEFGH1234ijklMNOP5678qrst";
    const GH_TOKEN: &str = "ghp_0123456789abcdefghijABCDEFGHIJ0123";

    #[test]
    fn masks_text() {
        struct Case {
            input: &'static str,
            o: Options,
            gone: &'static [&'static str],
            kept: &'static [&'static str],
            count: usize,
        }
        let secrets = Options {
            secrets: true,
            ..Default::default()
        };
        let personal = Options {
            secrets: true,
            personal: true,
            ..Default::default()
        };
        let cases = vec![
            Case {
                input: "export OPENAI_API_KEY=sk-proj-abcdEFGH1234ijklMNOP5678qrst",
                o: Options::default(),
                gone: &[OPENAI_KEY],
                kept: &["export OPENAI_API_KEY="],
                count: 1,
            },
            Case {
                input: "token ghp_0123456789abcdefghijABCDEFGHIJ0123 and AKIAIOSFODNN7EXAMPLE",
                o: Options::default(),
                gone: &[GH_TOKEN, "AKIAIOSFODNN7EXAMPLE"],
                kept: &[],
                count: 2,
            },
            Case {
                input: "postgres://app:s3cretPass@db.internal:5432/app",
                o: Options::default(),
                gone: &["s3cretPass"],
                kept: &["postgres://app:", "@db.internal"],
                count: 1,
            },
            Case {
                input: "postgres://app:${DB_PASS}@db/app",
                o: Options::default(),
                gone: &[],
                kept: &["${DB_PASS}"],
                count: 0,
            },
            Case {
                input: "DB_PASSWORD=\"hunter2hunter2x9\"",
                o: Options::default(),
                gone: &["hunter2hunter2x9"],
                kept: &[],
                count: 1,
            },
            Case {
                input: "password = getPassword()",
                o: Options::default(),
                gone: &[],
                kept: &["getPassword()"],
                count: 0,
            },
            Case {
                input: "the tokenizer splits words",
                o: Options::default(),
                gone: &[],
                kept: &["tokenizer"],
                count: 0,
            },
            Case {
                input: "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----",
                o: Options::default(),
                gone: &["MIIEow"],
                kept: &[],
                count: 1,
            },
            Case {
                input: "mail me: jane.doe@acme.io",
                o: Options::default(),
                gone: &[],
                kept: &["jane.doe@acme.io"],
                count: 0,
            },
            Case {
                input: "mail me: jane.doe@acme.io or 13812345678",
                o: personal.clone(),
                gone: &["jane.doe@acme.io", "13812345678"],
                kept: &[],
                count: 2,
            },
            Case {
                input: "see user@example.com",
                o: personal.clone(),
                gone: &[],
                kept: &["user@example.com"],
                count: 0,
            },
            Case {
                input: "id 11010519491231002X card 4111 1111 1111 1111",
                o: personal.clone(),
                gone: &["11010519491231002X", "4111 1111 1111 1111"],
                kept: &[],
                count: 2,
            },
            Case {
                input: "order 4111111111111112",
                o: personal.clone(),
                gone: &[],
                kept: &["4111111111111112"],
                count: 0,
            },
            Case {
                input: "Project Nightjar ships",
                o: Options {
                    words: vec!["Nightjar".to_owned()],
                    ..Default::default()
                },
                gone: &["Nightjar"],
                kept: &[],
                count: 1,
            },
        ];
        for c in &cases {
            let mut o = c.o.clone();
            if !o.personal && o.words.is_empty() {
                o.secrets = true;
            }
            let (out, n) = mask(c.input, &o);
            assert_eq!(n, c.count, "{}: {out}", c.input);
            for g in c.gone {
                assert!(!out.contains(g), "{}: {g} still in {out}", c.input);
            }
            for k in c.kept {
                assert!(out.contains(k), "{}: {k} gone from {out}", c.input);
            }
            assert_eq!(restore(&out, false), c.input, "round trip {}", c.input);
        }
        // the same value, the same placeholder
        let (a, _) = mask(&format!("k={OPENAI_KEY}"), &secrets);
        let (b, _) = mask(&format!("other {OPENAI_KEY}"), &secrets);
        let pa = find_placeholders(&a)
            .into_iter()
            .next()
            .map(|(x, y)| a[x..y].to_owned());
        let pb = find_placeholders(&b)
            .into_iter()
            .next()
            .map(|(x, y)| b[x..y].to_owned());
        assert!(pa.is_some(), "{a}");
        assert_eq!(pa, pb);
    }

    #[test]
    fn masks_json() {
        let body = format!(
            "{{\"model\":\"sk-proj-notmasked-because-model-key\",\"messages\":[{{\"role\":\"user\",\"content\":[\
             {{\"type\":\"text\",\"text\":\"key: {OPENAI_KEY}\\nok\"}},\
             {{\"type\":\"thinking\",\"thinking\":\"hm\",\"signature\":\"{OPENAI_KEY}\"}}]}}],\"n\":1,\"stream\":true}}"
        );
        let (out, n) = mask_json(
            body.as_bytes(),
            &Options {
                secrets: true,
                ..Default::default()
            },
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(n, 1, "{out}");
        assert_eq!(out.matches(OPENAI_KEY).count(), 1, "{out}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&out).is_ok(),
            "{out}"
        );
        assert!(
            out.contains("\"model\":\"sk-proj-notmasked-because-model-key\""),
            "{out}"
        );
        assert!(out.contains("\\nok\""), "{out}");
        assert_eq!(restore_json(out.as_bytes()), body.as_bytes());
        // untouched bodies come back byte for byte
        let plain = "{ \"a\" : [1, 2.5e3, true, null, \"xé\"] }";
        let (out, n) = mask_json(
            plain.as_bytes(),
            &Options {
                secrets: true,
                ..Default::default()
            },
        );
        assert_eq!(n, 0, "{}", String::from_utf8_lossy(&out));
        assert_eq!(out, plain.as_bytes());
    }

    // what the model writes back with a placeholder in it, streamed as vendors do
    #[test]
    fn writer_stream() {
        let masked = mask(
            OPENAI_KEY,
            &Options {
                secrets: true,
                ..Default::default()
            },
        )
        .0;
        let tool = format!("{{\"cmd\":\"echo {masked}\"}}");
        let text_event = |s: &str| {
            let data = json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": s}})
                .to_string();
            format!("event: content_block_delta\ndata: {data}\n\n")
        };
        let json_event = |s: &str| {
            let data =
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": s}})
                    .to_string();
            format!("event: content_block_delta\ndata: {data}\n\n")
        };
        let events = vec![
            text_event(&format!("use {}", &masked[..5])),
            text_event(&masked[5..12]),
            text_event(&format!("{} now {{", &masked[12..])),
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n"
                .to_owned(),
            json_event(&tool[..14]),
            json_event(&tool[14..]),
            "data: [DONE]\n\n".to_owned(),
        ];
        let all = events.concat();
        let mut w = Writer::new(Some("text/event-stream"), None);
        let mut body = Vec::new();
        // in awkward pieces
        for chunk in all.as_bytes().chunks(7) {
            body.extend_from_slice(&w.write(chunk));
        }
        body.extend_from_slice(&w.finish());
        let body = String::from_utf8(body).unwrap();
        let (mut text, mut js) = (String::new(), String::new());
        for ev in body.split("\n\n") {
            let Some((_, d)) = ev.split_once("data: ") else {
                continue;
            };
            if d == "[DONE]" {
                continue;
            }
            let e: serde_json::Value = serde_json::from_str(d).unwrap();
            text.push_str(e["delta"]["text"].as_str().unwrap_or_default());
            js.push_str(e["delta"]["partial_json"].as_str().unwrap_or_default());
        }
        assert_eq!(text, format!("use {OPENAI_KEY} now {{"));
        let args: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(args["cmd"], format!("echo {OPENAI_KEY}"), "{js}");
        assert_eq!(body.matches("\n\n").count(), events.len(), "{body}");
    }

    #[test]
    fn writer_whole() {
        let masked = mask(
            GH_TOKEN,
            &Options {
                secrets: true,
                ..Default::default()
            },
        )
        .0;
        let b = json!({"output": [
            {"type": "function_call", "arguments": format!("{{\"t\":\"{masked}\"}}")},
            {"type": "message", "content": [{"type": "output_text", "text": format!("got {masked}")}]},
        ]})
        .to_string();
        let mut w = Writer::new(Some("application/json"), None);
        w.write(b.as_bytes());
        let out = w.finish();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("{{"), "{out}");
        assert!(out.contains(&format!("got {GH_TOKEN}")), "{out}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&out).is_ok(),
            "{out}"
        );
    }
}
