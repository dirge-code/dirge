use std::path::PathBuf;

pub mod plan;
pub mod transcript;

pub struct LoopState {
    pub active: bool,
    pub prompt: String,
    pub plan_file: PathBuf,
    pub iteration: u32,
    pub max_iterations: Option<u32>,
    pub last_summary: Option<String>,
    pub run_cmd: Option<String>,
    pub last_run_output: Option<String>,
}

impl LoopState {
    pub fn new(
        prompt: String,
        plan_file: PathBuf,
        max_iterations: Option<u32>,
        run_cmd: Option<String>,
    ) -> Self {
        LoopState {
            active: true,
            prompt,
            plan_file,
            iteration: 0,
            max_iterations,
            last_summary: None,
            run_cmd,
            last_run_output: None,
        }
    }

    pub fn build_prompt(&self) -> String {
        let plan_contents = plan::read_plan(&self.plan_file).unwrap_or_default();

        let max_label = match self.max_iterations {
            Some(max) => max.to_string(),
            None => "∞".to_string(),
        };

        let summary = self.last_summary.as_deref().unwrap_or("starting fresh");
        let run_output = self.last_run_output.as_deref().unwrap_or("(none)");

        format!(
            "{}\n\n--- Loop Context (Iteration {}/{}) ---\n\nCurrent plan ({}):\n{}\n\nPrevious iteration summary:\n{}\n\nPrevious validation output:\n{}\n\n--- Instructions ---\n- Choose ONE task from the plan. Do not implement multiple things.\n- Before writing code, search the codebase with grep/find_files first.\n- After implementing: run the tests for the changed code.\n- Keep LOOP_PLAN.md up to date: mark completed items, add new findings.\n- If you discover bugs unrelated to your task, document them in LOOP_PLAN.md.\n- Commit working changes with descriptive messages.",
            self.prompt,
            self.iteration,
            max_label,
            self.plan_file.display(),
            plan_contents,
            summary,
            run_output,
        )
    }

    pub fn iteration_label(&self) -> String {
        let max_label = match self.max_iterations {
            Some(max) => max.to_string(),
            None => "∞".to_string(),
        };
        format!("LOOP {}/{}", self.iteration, max_label)
    }

    pub fn should_stop(&self) -> bool {
        match self.max_iterations {
            Some(max) => self.iteration >= max,
            None => false,
        }
    }

    /// dirge-vpma.15: advance to the next iteration, or report that the
    /// loop is done. Returns `false` WITHOUT incrementing once the max is
    /// reached, so `--loop-max N` runs exactly N iterations. The previous
    /// caller incremented before the `should_stop` check, so N ran N-1
    /// times (and N=1 ran zero). `None` max = infinite.
    pub fn next_iteration(&mut self) -> bool {
        if self.should_stop() {
            return false;
        }
        self.iteration += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(max: Option<u32>) -> LoopState {
        LoopState::new(String::new(), PathBuf::from("plan.md"), max, None)
    }

    #[test]
    fn loop_max_one_runs_exactly_one_iteration() {
        let mut s = state(Some(1));
        assert!(s.next_iteration(), "first iteration runs");
        assert_eq!(s.iteration, 1);
        assert!(!s.next_iteration(), "second call stops");
        assert_eq!(s.iteration, 1, "iteration must not advance past max");
    }

    #[test]
    fn loop_max_three_runs_exactly_three() {
        let mut s = state(Some(3));
        assert!(s.next_iteration());
        assert!(s.next_iteration());
        assert!(s.next_iteration());
        assert!(!s.next_iteration());
        assert_eq!(s.iteration, 3);
    }

    #[test]
    fn loop_no_max_never_stops() {
        let mut s = state(None);
        for _ in 0..100 {
            assert!(s.next_iteration());
        }
        assert_eq!(s.iteration, 100);
    }
}
