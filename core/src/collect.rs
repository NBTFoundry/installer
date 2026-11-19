// SPDX-License-Identifier: MPL-2.0
//! "Collect everything": one read-only report of what a car run needs, over
//! SSH, so no USB stick, shell or separate downloads are needed. Each section
//! is one fixed command using only tools the car inventory ran on the head unit
//! (cat, ls, mount, uname, pidin, sloginfo, audioconn -l), with its own size
//! limit and timeout; a section that fails is noted and the rest still run.
//! Nothing here writes on the head unit.

use crate::bench::{LOG_MAX, ROOT, SYSTEM_LOG_MAX};

pub struct Section {
    pub name: &'static str,
    pub command: String,
    pub limit: usize,
    pub seconds: u64,
}

fn section(name: &'static str, command: String, limit: usize, seconds: u64) -> Section {
    Section {
        name,
        command,
        limit,
        seconds,
    }
}

/// The sections, in report order.
pub fn sections() -> Vec<Section> {
    let manager = format!("{ROOT}/manager");
    let state = format!("{ROOT}/state/manager");
    vec![
        section("system", "uname -a; mount".into(), 16 * 1024, 30),
        section(
            "debug log",
            format!("cat {manager}/debug.log.1 {manager}/debug.log 2>/dev/null; exit 0"),
            LOG_MAX,
            60,
        ),
        section("system log", "sloginfo".into(), SYSTEM_LOG_MAX, 300),
        // Read only: -l lists; only -b/-r build or remove a connection. As in
        // tools/car_inventory.sh: both places it may be (which one the car
        // uses is not established), stopped after 10 s if the car's
        // ConnectionMaster does not answer.
        section(
            "audio connections",
            "for a in /fs/sda0/opt/mm/bin/audioconn /opt/mm/bin/audioconn; do \
             if [ -x $a ]; then LD_PRELOAD= $a -l & p=$!; n=0; \
             while kill -0 $p 2>/dev/null && [ $n -lt 10 ]; do sleep 1; n=$((n+1)); done; \
             kill $p 2>/dev/null && echo audioconn -l stopped after 10 s; wait $p; exit; fi; \
             done; echo audioconn not found; exit 1"
                .into(),
            64 * 1024,
            20,
        ),
        section("processes", "pidin arg".into(), 256 * 1024, 30),
        section(
            "process manager",
            "cat /dev/starter/status/overview".into(),
            64 * 1024,
            15,
        ),
        section(
            "capture shim",
            format!(
                "cat {state}/shim-state; ls -l {state}/hmi.env {state}/libnbt-capture.so /opt/sys/etc/starter.cfg"
            ),
            4096,
            15,
        ),
        section(
            "manager files",
            format!("ls -lR {manager} {ROOT}/state {ROOT}/apps"),
            128 * 1024,
            30,
        ),
        section("shared memory", "ls -l /dev/shmem".into(), 16 * 1024, 15),
    ]
}

/// What one section's command produced.
pub struct Output<'a> {
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub exit: Option<u32>,
    pub truncated: bool,
}

/// The report's first line.
pub fn title(unix_seconds: u64) -> Vec<u8> {
    format!("NBT Manager collection, {unix_seconds} s since 1970 (installer clock)\n").into_bytes()
}

/// One section's output in the report.
pub fn append(report: &mut Vec<u8>, section: &Section, result: Result<Output<'_>, &str>) {
    report.extend_from_slice(
        format!("\n===== {} =====\n$ {}\n", section.name, section.command).as_bytes(),
    );
    match result {
        Ok(Output {
            stdout,
            stderr,
            exit,
            truncated,
        }) => {
            report.extend_from_slice(stdout);
            if !stdout.is_empty() && !stdout.ends_with(b"\n") {
                report.push(b'\n');
            }
            if !stderr.is_empty() {
                report.extend_from_slice(b"----- standard error -----\n");
                report.extend_from_slice(stderr);
                if !stderr.ends_with(b"\n") {
                    report.push(b'\n');
                }
            }
            let exit = exit.map_or("none".into(), |code| code.to_string());
            report.extend_from_slice(
                format!(
                    "----- exit {exit}{} -----\n",
                    if truncated {
                        ", cut at the size limit"
                    } else {
                        ""
                    }
                )
                .as_bytes(),
            );
        }
        Err(error) => {
            report.extend_from_slice(format!("----- not completed: {error} -----\n").as_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_read_only_and_quoted_once() {
        let all = sections();
        assert!(all.len() >= 8);
        for s in &all {
            // ssh.rs quotes each command once for the login shell.
            assert!(!s.command.contains('\''), "{}", s.command);
            for writer in [" rm ", ">", "mkdir", "chmod", "mount -u", " -b ", " -r "] {
                let probe = format!(" {} ", s.command);
                assert!(
                    !probe.contains(writer) || s.command.contains("2>/dev/null"),
                    "{writer} in {}",
                    s.command
                );
            }
            assert!(s.limit > 0 && s.seconds > 0);
        }
        let audio = all.iter().find(|s| s.name == "audio connections").unwrap();
        assert!(audio.command.contains("LD_PRELOAD= $a -l &"));
        assert!(!audio.command.contains(" -b") && !audio.command.contains(" -r"));
    }

    #[test]
    fn a_report_names_each_section_and_its_outcome() {
        let all = sections();
        let mut report = title(42);
        let output = |stdout, stderr, exit, truncated| Output {
            stdout,
            stderr,
            exit,
            truncated,
        };
        append(
            &mut report,
            &all[0],
            Ok(output(b"QNX\n", b"", Some(0), false)),
        );
        append(
            &mut report,
            &all[3],
            Ok(output(b"list", b"warn\n", Some(1), true)),
        );
        append(&mut report, &all[4], Err("timed out"));
        let text = String::from_utf8(report).unwrap();
        assert!(text.starts_with("NBT Manager collection, 42 s"));
        assert!(text.contains("===== system =====\n$ uname -a; mount\nQNX\n----- exit 0 -----\n"));
        assert!(text.contains(
            "list\n----- standard error -----\nwarn\n----- exit 1, cut at the size limit -----"
        ));
        assert!(
            text.contains("===== processes =====\n$ pidin arg\n----- not completed: timed out")
        );
    }
}
