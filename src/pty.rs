use mio::unix::SourceFd;
use nix::{fcntl, libc, pty, sys::signal, sys::wait, unistd, unistd::ForkResult};
use std::fs;
use std::io::{self, Read, Write};
use std::ops::Deref;
use std::os::fd::RawFd;
use std::os::unix::io::{AsRawFd, FromRawFd};
use termion::raw::IntoRawMode;

pub trait Recorder {
    fn start(&mut self, size: (u16, u16)) -> io::Result<()>;
    fn output(&mut self, data: &[u8]);
    fn input(&mut self, data: &[u8]);
}

pub fn exec<S: AsRef<str>, R: Recorder>(args: &[S], recorder: &mut R) -> anyhow::Result<i32> {
    let tty = open_tty()?;
    let winsize = get_tty_size(tty.as_raw_fd());
    recorder.start((winsize.ws_col, winsize.ws_row))?;
    let result = unsafe { pty::forkpty(Some(&winsize), None) }?;

    match result.fork_result {
        ForkResult::Parent { child } => {
            handle_parent(result.master.as_raw_fd(), tty, child, recorder)
        }

        ForkResult::Child => {
            handle_child(args)?;
            unreachable!();
        }
    }
}

fn open_tty() -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
}

fn get_tty_size(tty_fd: i32) -> pty::Winsize {
    let mut winsize = pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    unsafe { libc::ioctl(tty_fd, libc::TIOCGWINSZ, &mut winsize) };

    winsize
}

fn handle_parent<R: Recorder>(
    master_fd: RawFd,
    tty: fs::File,
    child: unistd::Pid,
    recorder: &mut R,
) -> anyhow::Result<i32> {
    let copy_result = copy(master_fd, tty, recorder);
    let wait_result = wait::waitpid(child, None);
    copy_result?;

    match wait_result {
        Ok(wait::WaitStatus::Exited(_pid, status)) => Ok(status),
        Ok(wait::WaitStatus::Signaled(_pid, signal, ..)) => Ok(128 + signal as i32),
        Ok(_) => Ok(1),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

const MASTER: mio::Token = mio::Token(0);
const TTY: mio::Token = mio::Token(1);
const BUF_SIZE: usize = 128 * 1024;

fn copy<R: Recorder>(master_fd: RawFd, tty: fs::File, recorder: &mut R) -> anyhow::Result<()> {
    let mut master = unsafe { fs::File::from_raw_fd(master_fd) };
    let mut poll = mio::Poll::new()?;
    let mut events = mio::Events::with_capacity(128);
    let mut master_source = SourceFd(&master_fd);
    let mut tty = tty.into_raw_mode()?;
    let tty_fd = tty.as_raw_fd();
    let mut tty_source = SourceFd(&tty_fd);
    let mut buf = [0u8; BUF_SIZE];
    let mut input: Vec<u8> = Vec::with_capacity(BUF_SIZE);
    let mut output: Vec<u8> = Vec::with_capacity(BUF_SIZE);
    let mut flush = false;

    set_non_blocking(&master_fd)?;
    set_non_blocking(&tty_fd)?;

    poll.registry()
        .register(&mut master_source, MASTER, mio::Interest::READABLE)?;

    poll.registry()
        .register(&mut tty_source, TTY, mio::Interest::READABLE)?;

    loop {
        poll.poll(&mut events, None).unwrap();

        for event in events.iter() {
            match event.token() {
                MASTER => {
                    if event.is_readable() {
                        let offset = output.len();
                        let read = read_all(&mut master, &mut buf, &mut output)?;

                        if read > 0 {
                            recorder.output(&output[offset..]);

                            poll.registry().reregister(
                                &mut tty_source,
                                TTY,
                                mio::Interest::READABLE | mio::Interest::WRITABLE,
                            )?;
                        }
                    }

                    if event.is_writable() {
                        let left = write_all(&mut master, &mut input)?;

                        if left == 0 {
                            poll.registry().reregister(
                                &mut master_source,
                                MASTER,
                                mio::Interest::READABLE,
                            )?;
                        }
                    }

                    if event.is_read_closed() {
                        poll.registry().deregister(&mut master_source)?;

                        if !output.is_empty() {
                            flush = true;
                        } else {
                            return Ok(());
                        }
                    }
                }

                TTY => {
                    if event.is_writable() {
                        let left = write_all(&mut tty, &mut output)?;

                        if left == 0 {
                            if flush {
                                return Ok(());
                            } else {
                                poll.registry().reregister(
                                    &mut tty_source,
                                    TTY,
                                    mio::Interest::READABLE,
                                )?;
                            }
                        }
                    }

                    if event.is_readable() {
                        let offset = input.len();
                        let read = read_all(&mut tty.deref(), &mut buf, &mut input)?;

                        if read > 0 {
                            recorder.input(&input[offset..]);

                            poll.registry().reregister(
                                &mut master_source,
                                MASTER,
                                mio::Interest::READABLE | mio::Interest::WRITABLE,
                            )?;
                        }
                    }

                    if event.is_read_closed() {
                        poll.registry().deregister(&mut tty_source).unwrap();
                        return Ok(());
                    }
                }

                _ => (),
            }
        }
    }
}

fn handle_child<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    use signal::{SigHandler, Signal};
    use std::ffi::{CString, NulError};

    let args = args
        .iter()
        .map(|s| CString::new(s.as_ref()))
        .collect::<Result<Vec<CString>, NulError>>()?;

    unsafe { signal::signal(Signal::SIGPIPE, SigHandler::SigDfl) }?;
    unistd::execvp(&args[0], &args)?;
    unsafe { libc::_exit(1) }
}

fn set_non_blocking(fd: &RawFd) -> Result<(), io::Error> {
    use fcntl::{fcntl, FcntlArg::*, OFlag};

    let flags = fcntl(*fd, F_GETFL)?;
    let mut oflags = OFlag::from_bits_truncate(flags);
    oflags |= OFlag::O_NONBLOCK;
    fcntl(*fd, F_SETFL(oflags))?;

    Ok(())
}

fn read_all<R: Read>(source: &mut R, buf: &mut [u8], out: &mut Vec<u8>) -> io::Result<usize> {
    let mut read = 0;

    loop {
        match source.read(buf) {
            Ok(0) => (),

            Ok(n) => {
                out.extend_from_slice(&buf[0..n]);
                read += n;
            }

            Err(_) => {
                break;
            }
        }
    }

    Ok(read)
}

fn write_all<W: Write>(sink: &mut W, data: &mut Vec<u8>) -> io::Result<usize> {
    let mut buf: &[u8] = data.as_ref();

    loop {
        match sink.write(buf) {
            Ok(0) => (),

            Ok(n) => {
                buf = &buf[n..];

                if buf.is_empty() {
                    break;
                }
            }

            Err(_) => {
                break;
            }
        }
    }

    let left = buf.len();

    if left == 0 {
        data.clear();
    } else {
        let rot = data.len() - left;
        data.rotate_left(rot);
        data.truncate(left);
    }

    Ok(left)
}