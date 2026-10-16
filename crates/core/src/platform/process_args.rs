//! Bounded decoding of macOS KERN_PROCARGS2 snapshots. Kept platform-neutral
//! so malformed snapshots and argument boundaries can be tested without macOS.

/// Return only the executable and exactly argc arguments. Bytes after argv
/// (which may contain environment data) are never decoded or returned.
pub(super) fn decode_procargs2(bytes: &[u8]) -> Option<(String, Vec<String>)> {
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|&n| n > 0)?;
    let mut remaining = bytes.get(4..)?;
    // Each argument must have at least its terminating NUL. Bound allocations
    // by the supplied snapshot, not the untrusted argc header.
    if argc > remaining.len() {
        return None;
    }
    let executable = take_string(&mut remaining)?;
    if executable.is_empty() {
        return None;
    }
    while remaining.first() == Some(&0) {
        remaining = &remaining[1..];
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        argv.push(take_string(&mut remaining)?);
    }
    if argv.first().is_none_or(String::is_empty) {
        return None;
    }
    Some((executable, argv))
}

fn take_string(bytes: &mut &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0)?;
    let value = std::str::from_utf8(&bytes[..end]).ok()?.to_string();
    *bytes = &bytes[end + 1..];
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(args: &[&str]) -> Vec<u8> {
        let mut bytes = (args.len() as i32).to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/Applications/Claude.app/Contents/MacOS/Claude\0\0\0");
        for arg in args {
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn preserves_boundaries_spaces_and_empty_arguments() {
        let args = [
            "Claude",
            "--user-data-dir=/p/With Spaces/desktop-data",
            "",
            "--enable-logging",
        ];
        let (_, decoded) = decode_procargs2(&snapshot(&args)).unwrap();
        assert_eq!(decoded, args);
    }

    #[test]
    fn stops_after_argc_without_decoding_environment_bytes() {
        let mut bytes = snapshot(&["Claude", "--user-data-dir=/p/Iso"]);
        bytes.extend_from_slice(b"UNRELATED=\xff\0--user-data-dir=/p/Other\0");
        let (_, decoded) = decode_procargs2(&bytes).unwrap();
        assert_eq!(decoded, ["Claude", "--user-data-dir=/p/Iso"]);
    }

    #[test]
    fn preserves_type_switches_without_dropping_candidates() {
        for args in [
            vec!["Claude", "--type="],
            vec!["Claude", "--type=renderer", "--type="],
            vec!["Claude", "--", "--type=renderer"],
        ] {
            let (_, decoded) = decode_procargs2(&snapshot(&args)).unwrap();
            assert_eq!(decoded, args);
        }
    }

    #[test]
    fn rejects_malformed_and_truncated_snapshots() {
        let complete = snapshot(&["Claude", "--user-data-dir=/p/Iso"]);
        for end in 0..complete.len() {
            assert!(decode_procargs2(&complete[..end]).is_none(), "end={end}");
        }
        for argc in [-1i32, 0, i32::MAX] {
            let mut bytes = complete.clone();
            bytes[..4].copy_from_slice(&argc.to_ne_bytes());
            assert!(decode_procargs2(&bytes).is_none());
        }
        let mut bytes = snapshot(&["Claude"]);
        bytes[4] = 0xff;
        assert!(decode_procargs2(&bytes).is_none());
        let mut bytes = snapshot(&["Claude", "argument"]);
        let last_argument_byte = bytes.len() - 2;
        bytes[last_argument_byte] = 0xff;
        assert!(decode_procargs2(&bytes).is_none());
        let mut empty_executable = 1i32.to_ne_bytes().to_vec();
        empty_executable.extend_from_slice(b"\0Claude\0");
        assert!(decode_procargs2(&empty_executable).is_none());
        let mut padding_only = 1i32.to_ne_bytes().to_vec();
        padding_only.extend_from_slice(b"/Applications/Claude.app/Contents/MacOS/Claude\0\0\0");
        assert!(decode_procargs2(&padding_only).is_none());
    }
}
