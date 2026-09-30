//! How far a run has got through the work it picked up.
//!
//! One issue can take an hour, and the log in between is every agent call of
//! every round, so nothing in it said whether the run was on its first item of
//! six or its last. A line after each item says how many are done, how many
//! are left, and how long it has been.
//!
//! The total grows as the run learns it. Triage decides how many issues of a
//! wave get worked, and a wave of follow-ups can join after that, so the line
//! never claims a total the run does not know yet.

use std::time::{Duration, Instant};

use crate::log;
use crate::model::IssueRun;
use crate::spend;

const BAR_WIDTH: usize = 20;

pub struct Progress {
    done: usize,
    total: usize,
    started: Instant,
}

impl Progress {
    pub fn new(total: usize) -> Self {
        Self {
            done: 0,
            total,
            started: Instant::now(),
        }
    }

    /// More items this run will work, once triage has said which.
    pub fn add(&mut self, items: usize) {
        self.total += items;
    }

    /// One item reached its end, whatever that end was.
    pub fn finished(&mut self, run: &IssueRun) {
        self.done += 1;
        self.total = self.total.max(self.done);
        log!("{}", self.line(run, self.started.elapsed()));
    }

    fn line(&self, run: &IssueRun, elapsed: Duration) -> String {
        let left = self.total - self.done;
        format!(
            "{} {} of {} done, #{} {}, {left} left, {} elapsed",
            bar(self.done, self.total),
            self.done,
            self.total,
            run.issue,
            run.status,
            spend::clock(elapsed)
        )
    }
}

fn bar(done: usize, total: usize) -> String {
    let filled = (done * BAR_WIDTH)
        .checked_div(total)
        .unwrap_or(0)
        .min(BAR_WIDTH);
    format!("[{}{}]", "#".repeat(filled), ".".repeat(BAR_WIDTH - filled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    fn run(issue: i64, status: Status) -> IssueRun {
        let mut run = IssueRun::new(issue, "title");
        run.status = status;
        run
    }

    #[test]
    fn the_bar_fills_in_proportion_and_never_overflows() {
        assert_eq!(format!("[{}]", ".".repeat(20)), bar(0, 5));
        assert_eq!(format!("[{}{}]", "#".repeat(8), ".".repeat(12)), bar(2, 5));
        assert_eq!(format!("[{}]", "#".repeat(20)), bar(5, 5));
        assert_eq!(format!("[{}]", "#".repeat(20)), bar(7, 5));
        assert_eq!(format!("[{}]", ".".repeat(20)), bar(0, 0));
    }

    #[test]
    fn a_line_names_the_item_just_finished_and_what_is_left() {
        let mut progress = Progress::new(5);
        progress.done = 2;
        let line = progress.line(&run(123, Status::Approved), Duration::from_secs(4000));
        assert!(
            line.ends_with("2 of 5 done, #123 approved, 3 left, 66m40s elapsed"),
            "{line}"
        );
    }

    #[test]
    fn the_total_grows_as_waves_join_and_never_trails_what_is_done() {
        let mut progress = Progress::new(1);
        progress.finished(&run(1, Status::Merged));
        progress.add(2);
        assert_eq!((1, 3), (progress.done, progress.total));
        progress.finished(&run(2, Status::Escalated));
        progress.finished(&run(3, Status::Approved));
        progress.finished(&run(4, Status::Error));
        assert_eq!((4, 4), (progress.done, progress.total));
    }
}
