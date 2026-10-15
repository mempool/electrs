use crossbeam_channel as channel;
use crossbeam_channel::RecvTimeoutError;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};

use crate::errors::*;

#[derive(Clone)] // so multiple threads could wait on signals
pub struct Waiter {
    receiver: channel::Receiver<i32>,
    // A termination signal is delivered through the channel only once, but
    // several threads and loops wait on clones of this Waiter. Remember it so
    // every later wait() also fails, even if the first receiver swallowed the
    // error (e.g. a failed mempool update that only logs and continues).
    interrupted: Arc<AtomicI32>,
}

fn notify(signals: &[i32]) -> channel::Receiver<i32> {
    let (s, r) = channel::bounded(1);
    let mut signals =
        signal_hook::iterator::Signals::new(signals).expect("failed to register signal hook");
    crate::util::spawn_thread("signal-notifier", move || {
        for signal in signals.forever() {
            s.send(signal)
                .unwrap_or_else(|_| panic!("failed to send signal {}", signal));
        }
    });
    r
}

impl Waiter {
    pub fn start() -> Waiter {
        Waiter::from_receiver(notify(&[
            SIGINT, SIGTERM,
            SIGUSR1, // allow external triggering (e.g. via bitcoind `blocknotify`)
        ]))
    }

    fn from_receiver(receiver: channel::Receiver<i32>) -> Waiter {
        Waiter {
            receiver,
            interrupted: Arc::new(AtomicI32::new(0)),
        }
    }

    pub fn wait(&self, duration: Duration, accept_sigusr: bool) -> Result<()> {
        // Determine the deadline time based on the duration, so that it doesn't
        // get pushed back when wait_deadline() recurses
        self.wait_deadline(Instant::now() + duration, accept_sigusr)
    }

    fn wait_deadline(&self, deadline: Instant, accept_sigusr: bool) -> Result<()> {
        let sig = self.interrupted.load(Ordering::SeqCst);
        if sig != 0 {
            bail!(ErrorKind::Interrupt(sig));
        }
        match self.receiver.recv_deadline(deadline) {
            Ok(sig) if sig == SIGUSR1 => {
                trace!("notified via SIGUSR1");
                if accept_sigusr {
                    Ok(())
                } else {
                    self.wait_deadline(deadline, accept_sigusr)
                }
            }
            Ok(sig) => {
                self.interrupted.store(sig, Ordering::SeqCst);
                bail!(ErrorKind::Interrupt(sig))
            }
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => bail!("signal hook channel disconnected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_interrupt(res: Result<()>, expected: i32) -> bool {
        matches!(res, Err(Error(ErrorKind::Interrupt(sig), _)) if sig == expected)
    }

    #[test]
    fn interrupt_is_seen_by_every_later_wait() {
        let (s, r) = channel::bounded(1);
        let waiter = Waiter::from_receiver(r);
        let other = waiter.clone();
        s.send(SIGTERM).unwrap();

        // The first wait consumes the signal from the channel...
        assert!(is_interrupt(
            waiter.wait(Duration::from_secs(1), false),
            SIGTERM
        ));
        // ...but later waits, on any clone, must still report it immediately.
        let start = Instant::now();
        assert!(is_interrupt(
            waiter.wait(Duration::from_secs(5), false),
            SIGTERM
        ));
        assert!(is_interrupt(
            other.wait(Duration::from_secs(5), true),
            SIGTERM
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn sigusr1_is_not_sticky() {
        let (s, r) = channel::bounded(1);
        let waiter = Waiter::from_receiver(r);
        s.send(SIGUSR1).unwrap();
        assert!(waiter.wait(Duration::from_secs(1), true).is_ok());
        assert!(waiter.wait(Duration::from_millis(50), true).is_ok());
    }
}
