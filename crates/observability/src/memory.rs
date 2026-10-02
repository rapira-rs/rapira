/// The memory of one worker, in bytes.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Memory {
    pub rss: Option<u64>,
    pub pss: Option<u64>,
}

/// Reads `/proc/<pid>/smaps_rollup`: https://docs.kernel.org/filesystems/proc.html#process-specific-subdirectories. Both values are None when the process is gone or the file cannot be read.
#[cfg(target_os = "linux")]
pub(crate) fn read(pid: u32) -> Memory {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .map_or_else(|_| Memory::default(), |text| parse(&text))
}

/// Only Linux has `/proc/<pid>/smaps_rollup`.
#[cfg(not(target_os = "linux"))]
pub(crate) fn read(_pid: u32) -> Memory {
    Memory::default()
}

/// The `Rss:` and `Pss:` lines. The kernel writes the values in kB.
#[cfg(any(target_os = "linux", test))]
fn parse(text: &str) -> Memory {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let kb: u64 = line
                .strip_prefix(name)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse()
                .ok()?;
            Some(kb * 1024)
        })
    };
    Memory {
        rss: field("Rss:"),
        pss: field("Pss:"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_rss_and_pss_in_bytes() {
        struct Case {
            name: &'static str,
            text: &'static str,
            want: Memory,
        }
        let cases = [
            Case {
                name: "rollup",
                text: "55d0c3a4c000-7ffd4b5f1000 ---p 00000000 00:00 0    [rollup]\n\
                       Rss:               12288 kB\n\
                       Pss:                4100 kB\n\
                       Pss_Anon:           3000 kB\n\
                       Pss_File:           1100 kB\n",
                want: Memory {
                    rss: Some(12288 * 1024),
                    pss: Some(4100 * 1024),
                },
            },
            Case {
                name: "no pss line",
                text: "Rss: 8 kB\n",
                want: Memory {
                    rss: Some(8192),
                    pss: None,
                },
            },
            Case {
                name: "pss_anon is not pss",
                text: "Pss_Anon: 3000 kB\n",
                want: Memory {
                    rss: None,
                    pss: None,
                },
            },
        ];
        for case in cases {
            assert_eq!(parse(case.text), case.want, "{}", case.name);
        }
    }
}
