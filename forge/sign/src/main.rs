//! Signs PROTEUS release manifests with Ed25519.
//!
//!   proteus-sign keygen KEYFILE        create a new secret key (refuses to overwrite)
//!   proteus-sign public KEYFILE        print the public key as hex (this goes into the exe)
//!   proteus-sign sign KEYFILE FILE     write FILE.sig (hex) next to FILE
//!   proteus-sign verify PUBHEX FILE    check FILE against FILE.sig
//!
//! The secret key is 32 bytes as hex in KEYFILE. Keep it on the release
//! machine only: whoever holds it can ship code to every PROTEUS that updates.
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::{env, fs, process::exit};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn key(path: &str) -> SigningKey {
    let bytes = unhex(&fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}")))).unwrap_or_else(|| die("key file is not hex"));
    let arr: [u8; 32] = bytes.try_into().unwrap_or_else(|_| die("key must be 32 bytes"));
    SigningKey::from_bytes(&arr)
}

fn die(msg: &str) -> ! {
    eprintln!("proteus-sign: {msg}");
    exit(1)
}

fn main() {
    let a: Vec<String> = env::args().skip(1).collect();
    match a.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["keygen", path] => {
            if fs::metadata(path).is_ok() {
                die("refusing to overwrite an existing key");
            }
            let k = SigningKey::generate(&mut rand_core::OsRng);
            fs::write(path, hex(&k.to_bytes())).unwrap_or_else(|e| die(&e.to_string()));
            println!("{}", hex(k.verifying_key().as_bytes()));
        }
        ["public", path] => println!("{}", hex(key(path).verifying_key().as_bytes())),
        ["sign", path, file] => {
            let data = fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
            let sig = key(path).sign(&data);
            fs::write(format!("{file}.sig"), hex(&sig.to_bytes())).unwrap_or_else(|e| die(&e.to_string()));
            println!("signed {file} ({} bytes)", data.len());
        }
        ["verify", pubhex, file] => {
            let pk: [u8; 32] = unhex(pubhex).and_then(|v| v.try_into().ok()).unwrap_or_else(|| die("public key must be 32 bytes of hex"));
            let pk = VerifyingKey::from_bytes(&pk).unwrap_or_else(|_| die("not a valid public key"));
            let sig: [u8; 64] = fs::read_to_string(format!("{file}.sig")).ok().and_then(|s| unhex(&s)).and_then(|v| v.try_into().ok()).unwrap_or_else(|| die("no usable .sig beside the file"));
            let data = fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
            match pk.verify(&data, &Signature::from_bytes(&sig)) {
                Ok(()) => println!("ok"),
                Err(_) => die("signature does not match"),
            }
        }
        _ => die("usage: keygen KEYFILE | public KEYFILE | sign KEYFILE FILE | verify PUBHEX FILE"),
    }
}
