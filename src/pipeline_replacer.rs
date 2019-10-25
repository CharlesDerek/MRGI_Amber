use crate::console::{Console, ConsoleTextKind};
use crate::pipeline::{Pipeline, PipelineInfo};
use crate::pipeline_matcher::PathMatch;
use crate::util::{catch, decode_error, exit};
use crossbeam_channel::{Receiver, Sender};
use ctrlc;
use getch::Getch;
use memmap::Mmap;
use regex::Regex;
use std::fs::{self, File};
use std::io::{Error, Write};
use std::ops::Deref;
use std::str;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------------------------------------------------
// PipelineReplacer
// ---------------------------------------------------------------------------------------------------------------------

pub struct PipelineReplacer {
    pub is_color: bool,
    pub is_interactive: bool,
    pub print_file: bool,
    pub print_column: bool,
    pub print_row: bool,
    pub infos: Vec<String>,
    pub errors: Vec<String>,
    console: Console,
    all_replace: bool,
    keyword: Vec<u8>,
    replacement: Vec<u8>,
    regex: bool,
    time_beg: Instant,
    time_bsy: Duration,
}

impl PipelineReplacer {
    pub fn new(keyword: &[u8], replacement: &[u8], regex: bool) -> Self {
        PipelineReplacer {
            is_color: true,
            is_interactive: true,
            print_file: true,
            print_column: false,
            print_row: false,
            infos: Vec::new(),
            errors: Vec::new(),
            console: Console::new(),
            all_replace: false,
            keyword: Vec::from(keyword),
            replacement: Vec::from(replacement),
            regex: regex,
            time_beg: Instant::now(),
            time_bsy: Duration::new(0, 0),
        }
    }

    fn replace_match(&mut self, pm: PathMatch) {
        if pm.matches.is_empty() {
            return;
        }
        self.console.is_color = self.is_color;

        let result = catch::<_, (), Error>(|| {
            let mut tmpfile = NamedTempFile::new_in(pm.path.parent().unwrap_or(&pm.path))?;

            let tmpfile_path = tmpfile.path().to_path_buf();
            let _ = ctrlc::set_handler(move || {
                let path = tmpfile_path.clone();
                let mut console = Console::new();
                console.write(
                    ConsoleTextKind::Info,
                    &format!("\nCleanup temporary file: {:?}\n", path),
                );
                let _ = fs::remove_file(path);
                exit(0, &mut console);
            });

            {
                let file = File::open(&pm.path)?;
                let mmap = unsafe { Mmap::map(&file) }?;
                let src = mmap.deref();

                let mut i = 0;
                let mut pos = 0;
                let mut column = 0;
                let mut last_lf = 0;
                for m in &pm.matches {
                    tmpfile.write_all(&src[i..m.beg])?;

                    let replacement = if self.regex {
                        self.get_regex_replacement(&src[m.beg..m.end])
                    } else {
                        self.replacement.clone()
                    };

                    let mut do_replace = true;
                    if self.is_interactive & !self.all_replace {
                        let mut header_witdh = 0;
                        if self.print_file {
                            let path = pm.path.to_str().unwrap();
                            header_witdh += UnicodeWidthStr::width(path) + 2;
                            self.console.write(ConsoleTextKind::Filename, path);
                            self.console.write(ConsoleTextKind::Other, ": ");
                        }
                        if self.print_column | self.print_row {
                            while pos < m.beg {
                                if src[pos] == 0x0a {
                                    column += 1;
                                    last_lf = pos;
                                }
                                pos += 1;
                            }
                            if self.print_column {
                                let column_str = format!("{}:", column + 1);
                                header_witdh += column_str.width();
                                self.console.write(ConsoleTextKind::Other, &column_str);
                            }
                            if self.print_row {
                                let row_str = format!("{}:", m.beg - last_lf);
                                header_witdh += row_str.width();
                                self.console.write(ConsoleTextKind::Other, &row_str);
                            }
                        }

                        if header_witdh < 4 {
                            self.console
                                .write(ConsoleTextKind::Other, &format!("{}", " ".repeat(4 - header_witdh)));
                            header_witdh = 4;
                        }

                        self.console.write_match_line(src, m);
                        self.console
                            .write(ConsoleTextKind::Other, &format!("{} -> ", " ".repeat(header_witdh - 4)));
                        self.console.write_replace_line(src, m, &replacement);

                        let getch = Getch::new();
                        loop {
                            self.console
                                .write(ConsoleTextKind::Other, "Replace keyword? [Y]es/[n]o/[a]ll/[q]uit: ");
                            self.console.flush();
                            let key = char::from(getch.getch()?);
                            if key != '\n' {
                                self.console.write(ConsoleTextKind::Other, &format!("{}\n", key));
                            } else {
                                self.console.write(ConsoleTextKind::Other, "\n");
                            }
                            match key {
                                'Y' | 'y' | ' ' | '\r' | '\n' => do_replace = true,
                                'N' | 'n' => do_replace = false,
                                'A' | 'a' => self.all_replace = true,
                                'Q' | 'q' => {
                                    let _ = tmpfile.close();
                                    exit(0, &mut self.console);
                                }
                                _ => continue,
                            }
                            break;
                        }
                    }

                    if do_replace {
                        tmpfile.write_all(&replacement)?;
                    } else {
                        tmpfile.write_all(&src[m.beg..m.end])?;
                    }
                    i = m.end;
                }

                if i < src.len() {
                    tmpfile.write_all(&src[i..src.len()])?;
                }
                tmpfile.flush()?;
            }

            let real_path = fs::canonicalize(&pm.path)?;

            let metadata = fs::metadata(&real_path)?;

            fs::set_permissions(tmpfile.path(), metadata.permissions())?;
            tmpfile.persist(&real_path)?;

            Ok(())
        });
        match result {
            Ok(_) => (),
            Err(e) => self.console.write(
                ConsoleTextKind::Error,
                &format!("Error: {} @ {:?}\n", decode_error(e.kind()), pm.path),
            ),
        }
    }

    fn get_regex_replacement(&self, org: &[u8]) -> Vec<u8> {
        // All unwrap() is safe bacause keyword is already matched in pipeline_matcher
        let org = str::from_utf8(org).unwrap();
        let keyword = str::from_utf8(&self.keyword).unwrap();
        let replacement = str::from_utf8(&self.replacement).unwrap();
        let regex = Regex::new(&keyword).unwrap();
        let captures = regex.captures(&org).unwrap();

        let mut dst = String::new();
        captures.expand(&replacement, &mut dst);

        dst.into_bytes()
    }
}

impl Pipeline<PathMatch, ()> for PipelineReplacer {
    fn setup(&mut self, id: usize, rx: Receiver<PipelineInfo<PathMatch>>, tx: Sender<PipelineInfo<()>>) {
        self.infos = Vec::new();
        self.errors = Vec::new();
        let mut seq_beg_arrived = false;

        loop {
            match rx.recv() {
                Ok(PipelineInfo::SeqDat(x, pm)) => {
                    watch_time!(self.time_bsy, {
                        self.replace_match(pm);
                        let _ = tx.send(PipelineInfo::SeqDat(x, ()));
                    });
                }

                Ok(PipelineInfo::SeqBeg(x)) => {
                    if !seq_beg_arrived {
                        self.time_beg = Instant::now();
                        let _ = tx.send(PipelineInfo::SeqBeg(x));
                        seq_beg_arrived = true;
                    }
                }

                Ok(PipelineInfo::SeqEnd(x)) => {
                    for i in &self.infos {
                        let _ = tx.send(PipelineInfo::MsgInfo(id, i.clone()));
                    }
                    for e in &self.errors {
                        let _ = tx.send(PipelineInfo::MsgErr(id, e.clone()));
                    }

                    let _ = tx.send(PipelineInfo::MsgTime(id, self.time_bsy, self.time_beg.elapsed()));
                    let _ = tx.send(PipelineInfo::SeqEnd(x));
                    break;
                }

                Ok(PipelineInfo::MsgInfo(i, e)) => {
                    let _ = tx.send(PipelineInfo::MsgInfo(i, e));
                }
                Ok(PipelineInfo::MsgErr(i, e)) => {
                    let _ = tx.send(PipelineInfo::MsgErr(i, e));
                }
                Ok(PipelineInfo::MsgTime(i, t0, t1)) => {
                    let _ = tx.send(PipelineInfo::MsgTime(i, t0, t1));
                }
                Err(_) => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------------------------------------------------
