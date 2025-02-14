use std::num;
use rand::Rng;

pub mod constants;
pub use constants::*;
use std::net::IpAddr;

#[cfg(not(target_os = "macos"))]
use thread_priority::*;

/// Convert bytes array to HEX format
pub fn to_hex(buf: &[u8]) -> String {
    let mut result = String::new();
    for x in buf.iter() {
        result.push_str(&format!("{:01$X}", x, 2));
    }
    result
}

pub fn from_hex(string: &str) -> Result<Vec<u8>, num::ParseIntError> {
    split_n(&string.trim()[..], 2)
        .iter()
        .map(|b| u8::from_str_radix(b, 16))
        .collect()
}

pub fn check_domain(name: &str, allow_dots: bool) -> bool {
    if name.starts_with('.') || name.starts_with('-') || name.ends_with('.') || name.ends_with('-') {
        return false;
    }
    let mut last_dot = false;
    let mut last_hyphen = false;
    for char in name.chars() {
        if allow_dots && char == '.' {
            if last_dot {
                return false;
            } else {
                last_dot = true;
                continue;
            }
        }
        if char == '-' {
            if last_hyphen {
                return false;
            } else {
                last_hyphen = true;
                continue;
            }
        }
        last_dot = false;
        last_hyphen = false;
        if !char.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

pub fn get_domain_zone(domain: &str) -> String {
    let parts: Vec<&str> = domain.rsplitn(2, ".").collect();
    if !parts.is_empty() {
        parts[0].to_owned()
    } else {
        String::new()
    }
}

fn split_n(s: &str, n: usize) -> Vec<&str> {
    (0..=(s.len() - n + 1) / 2)
        .map(|i| &s[2 * i..2 * i + n])
        .collect()
}

/// Generates random string of given length
pub fn random_string(length: usize) -> String {
    let chars: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!?".chars().collect();
    let mut rng = rand::thread_rng();
    let mut result = String::with_capacity(length);
    for _ in 0..length {
        let position: usize = rng.gen::<usize>() % chars.len();
        let c: char = *chars.get(position).unwrap();
        result.push(c);
    }
    result
}

/// Checks if this IP is from Yggdrasil network
/// https://yggdrasil-network.github.io
pub fn is_yggdrasil(addr: &IpAddr) -> bool {
    if let IpAddr::V6(ipv6) = addr {
        let first_byte = ipv6.octets()[0];
        return first_byte == 2 || first_byte == 3;
    }
    false
}

#[cfg(target_os = "windows")]
#[allow(unused_variables)]
pub fn setup_miner_thread(cpu: u32) {
    let _ = set_current_thread_priority(ThreadPriority::Min);
    //let _ = set_current_thread_ideal_processor(IdealProcessor::from(cpu));
}

#[cfg(target_os = "linux")]
#[allow(unused_variables)]
pub fn setup_miner_thread(cpu: u32) {
    let _ = set_current_thread_priority(ThreadPriority::Min);
}

#[cfg(target_os = "macos")]
#[allow(unused_variables)]
pub fn setup_miner_thread(cpu: u32) {
    // MacOS is not supported by thread_priority crate
}

#[cfg(test)]
mod test {
    use crate::{check_domain, is_yggdrasil};
    use std::net::IpAddr;

    #[test]
    fn test_check_domain() {
        assert!(check_domain("abc0", false));
        assert!(!check_domain("ab.c", false));
        assert!(check_domain("a.b.c", true));
        assert!(!check_domain("ab..c", true));
        assert!(check_domain("a-b.c", true));
        assert!(!check_domain("a--b.c", true));
        assert!(check_domain("a-0-b.c", true));
        assert!(!check_domain("-ab.c", true));
        assert!(!check_domain("ab.c-", true));
        assert!(!check_domain(".ab.c", true));
        assert!(!check_domain("ab.c-", true));
    }

    #[test]
    fn test_is_yggdrasil() {
        let addr: IpAddr = "200::1".parse().unwrap();
        assert!(is_yggdrasil(&addr));
        let addr: IpAddr = "226::1".parse().unwrap();
        assert!(is_yggdrasil(&addr));
        let addr: IpAddr = "300::1".parse().unwrap();
        assert!(is_yggdrasil(&addr));
        let addr: IpAddr = "326::1".parse().unwrap();
        assert!(is_yggdrasil(&addr));
        let addr: IpAddr = "2001::1".parse().unwrap();
        assert!(!is_yggdrasil(&addr));
        let addr: IpAddr = "2201::1".parse().unwrap();
        assert!(!is_yggdrasil(&addr));
    }
}
