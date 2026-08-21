//! Detect a model reciting its own system prompt back at the user (dirge-e31n.6).
//!
//! # What this catches
//!
//! Some models, under a long or confusing context, stop answering and start
//! replaying their instructions — a paragraph or several of the system prompt
//! rendered as if it were output. It wastes the whole turn, it is confusing to
//! read, and nothing else in the harness sees it: `storm` watches tool calls,
//! the failure tracker watches errors, and both are silent because neither is
//! happening.
//!
//! # The only hard part is NOT firing
//!
//! A detector that trips on a model legitimately quoting one line of its
//! instructions ("you asked me to always run tests, so:") is worse than no
//! detector — it breaks working sessions for a cosmetic problem, and the first
//! false positive is the last time anyone leaves it on.
//!
//! So the trigger is a RUN, not a count. One quoted line produces one or two
//! matching windows; a recitation produces an unbroken sequence of them. The
//! run length is the whole discrimination, and
//! [`tests::quoting_a_single_prompt_line_does_not_trip`] is the test that
//! matters most in this file. Everything else is bookkeeping around it.
//!
//! # Method
//!
//! SimHash-64 over sliding word windows, the same primitive
//! `llmtrim::stages::dedup` already uses for near-duplicate lines. The system
//! prompt is hashed once into a set of window signatures; the streamed output
//! is hashed the same way as it arrives; a window whose Hamming distance to ANY
//! prompt window is within [`MAX_DISTANCE`] counts as a match.
//!
//! Word windows rather than character windows because the hasher tokenises to
//! words anyway, and because a character window starting mid-word produces a
//! different token set for identical text.
//!
//! # Stopwords are dropped, and that is not an optimisation
//!
//! The first cut hashed raw words and false-positived on ordinary prose —
//! [`tests::an_interrupted_run_does_not_accumulate`] caught it with a run of 6.
//! The reason is structural: in a 24-word English window, ten or more tokens
//! are `the`/`and`/`in`/`a`, so two windows about entirely different subjects
//! share most of their signature and land inside a Hamming-8 radius. A detector
//! keyed on that measures "is this English", not "is this my prompt".
//!
//! So windows are built from CONTENT words only, using the same
//! language-detected stopword set `llmtrim` already uses. A window is then 16
//! content words — roughly 30-40 words of running text — and a match means the
//! distinctive vocabulary lined up, not the grammar.

use gaoya::simhash::{SimHashBits, SimSipHasher64};

use crate::llmtrim::stages::dedup::make_simhasher;

/// CONTENT words per window (stopwords excluded — see the module docs). 16
/// content words is roughly 30-40 words of running text: long enough that an
/// incidental shared phrase cannot fill one, short enough that a recited
/// paragraph produces several.
pub const WINDOW_WORDS: usize = 16;

/// Maximum Hamming distance between two 64-bit signatures for a match.
/// 8 of 64 bits — the value the reference design used, and loose enough to
/// survive the model reflowing whitespace or changing a word.
pub const MAX_DISTANCE: usize = 8;

/// Consecutive matching windows required to call it a recitation.
///
/// This is the false-positive guard, and the number is MEASURED, not reasoned.
/// The first cut used 6 on the argument that it was "far past anything a
/// quotation produces". It was not:
/// [`tests::a_verbatim_quote_below_the_run_threshold_does_not_trip`] showed an
/// 18-content-word quote — two ordinary sentences repeated word for word —
/// reaching a run of 6 and tripping. Windows straddling the quote's edges mix
/// in surrounding prose and still land inside the Hamming radius, so the run a
/// quote produces is larger than its inner-window count suggests.
///
/// At 12 a quote must reach roughly 28 consecutive content words — about 55
/// words of running text reproduced verbatim — before it trips, while a full
/// recitation of a realistic system prompt produces 40+ and trips easily. That
/// is the margin; it is smaller than it looks, and lowering this without
/// re-running those fixtures will start breaking working sessions.
pub const TRIP_RUN: usize = 12;

/// A detected recitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leak {
    /// Byte offset in the output where the matching run began. The text before
    /// it is the model's real answer and is worth keeping.
    pub start_offset: usize,
    /// How many consecutive windows matched by the time it tripped.
    pub run: usize,
}

/// Streaming detector. Built once per turn from the system prompt, then fed the
/// cumulative output text as it arrives.
#[derive(Debug, Clone)]
pub struct PromptLeakDetector {
    /// One signature per system-prompt window.
    prompt_sigs: Vec<u64>,
    /// The last [`WINDOW_WORDS`] output words, as a ring. Bounded, so a long
    /// answer does not grow this.
    recent: std::collections::VecDeque<String>,
    /// Byte offset of the first word of each word in `recent`, so a trip can
    /// report where the run started in the ORIGINAL text.
    recent_offsets: std::collections::VecDeque<usize>,
    /// Bytes of cumulative text already tokenised.
    consumed: usize,
    /// Length of the current unbroken run of matching windows.
    run: usize,
    /// Offset where the current run started.
    run_start: usize,
    /// Latched once tripped, so a caller polling every delta gets one answer.
    tripped: Option<Leak>,
    /// High-water mark of [`Self::run`]. Exists so a false-positive test can
    /// assert its fixture actually PRODUCED matching windows — without that,
    /// a fixture that matches nothing passes for the wrong reason and the run
    /// gate goes untested.
    max_run_seen: usize,
    /// Every matching window, run or not. Lets a test assert that separated
    /// quotes WOULD sum past the threshold, so a green result is attributable
    /// to the reset rather than to the fixture matching too little.
    total_matches: usize,
    /// Stopword set, detected ONCE from the system prompt. See
    /// [`word_offsets`] for why it is not re-detected per delta.
    stops: &'static std::collections::HashSet<&'static str>,
}

impl PromptLeakDetector {
    /// Build from the system prompt. Returns `None` when the prompt is too
    /// short to form a single window — there is nothing to recite, and a
    /// detector with no signatures would match nothing anyway.
    pub fn new(system_prompt: &str) -> Option<Self> {
        let stops = crate::llmtrim::stages::tools::stopword_set(system_prompt);
        let sigs = window_signatures(system_prompt, stops);
        if sigs.is_empty() {
            return None;
        }
        Some(Self {
            stops,
            prompt_sigs: sigs,
            recent: std::collections::VecDeque::with_capacity(WINDOW_WORDS),
            recent_offsets: std::collections::VecDeque::with_capacity(WINDOW_WORDS),
            consumed: 0,
            run: 0,
            run_start: 0,
            tripped: None,
            max_run_seen: 0,
            total_matches: 0,
        })
    }

    /// Feed the CUMULATIVE output text. Safe to call on every delta: the
    /// detector tracks how much it has already tokenised and only processes
    /// the new suffix, so this is linear over the turn rather than quadratic.
    ///
    /// Returns the leak on the delta that trips it, and on every delta after
    /// (latched), so a caller that checks the return value cannot miss it.
    pub fn observe(&mut self, cumulative: &str) -> Option<Leak> {
        if self.tripped.is_some() {
            return self.tripped;
        }
        // A shorter string than we have consumed means the caller restarted
        // (a retried turn reuses the detector). Reset rather than panic on the
        // slice — a stale run carried into a fresh attempt would be a
        // false positive built out of two different outputs.
        if cumulative.len() < self.consumed {
            self.reset();
        }
        let fresh = &cumulative[self.consumed..];
        // Only consume up to the last whitespace: the tail of a streamed chunk
        // is usually half a word, and tokenising it now would hash a fragment
        // and then hash the whole word again next delta.
        let take = match fresh.rfind(char::is_whitespace) {
            Some(i) => i + 1,
            // No whitespace at all — hold everything until a boundary arrives,
            // unless this is an unreasonably long unbroken token.
            None if fresh.len() < 4096 => return None,
            None => fresh.len(),
        };
        let chunk_start = self.consumed;
        let chunk = &fresh[..take];
        self.consumed += take;

        for (rel, word) in word_offsets(chunk, self.stops) {
            self.push_word(word, chunk_start + rel);
            if self.recent.len() < WINDOW_WORDS {
                continue;
            }
            let sig = self.window_signature();
            if self.matches_prompt(sig) {
                if self.run == 0 {
                    self.run_start = *self.recent_offsets.front().unwrap_or(&0);
                }
                self.run += 1;
                self.max_run_seen = self.max_run_seen.max(self.run);
                self.total_matches += 1;
                if self.run >= TRIP_RUN {
                    self.tripped = Some(Leak {
                        start_offset: self.run_start,
                        run: self.run,
                    });
                    return self.tripped;
                }
            } else {
                self.run = 0;
            }
        }
        None
    }

    /// The leak, if this detector has tripped. Not used by the stream path,
    /// which acts on [`Self::observe`]'s return value; kept so a caller that
    /// polls rather than reacts can ask.
    #[allow(dead_code)]
    pub fn leak(&self) -> Option<Leak> {
        self.tripped
    }

    fn reset(&mut self) {
        self.recent.clear();
        self.recent_offsets.clear();
        self.consumed = 0;
        self.run = 0;
        self.run_start = 0;
    }

    fn push_word(&mut self, word: String, offset: usize) {
        if self.recent.len() == WINDOW_WORDS {
            self.recent.pop_front();
            self.recent_offsets.pop_front();
        }
        self.recent.push_back(word);
        self.recent_offsets.push_back(offset);
    }

    fn window_signature(&self) -> u64 {
        let hasher = make_simhasher();
        hasher.create_signature(self.recent.iter())
    }

    fn matches_prompt(&self, sig: u64) -> bool {
        self.prompt_sigs
            .iter()
            .any(|p| p.hamming_distance(&sig) <= MAX_DISTANCE)
    }
}

/// Lowercased CONTENT words of `text` with the byte offset each starts at.
///
/// The offsets are what let a trip report where in the ORIGINAL output the
/// recitation began, so the caller can keep the real answer that preceded it.
///
/// `stops` is passed in rather than detected per call: detection runs
/// `whatlang` over the text, and doing that on every streamed delta would both
/// cost real time and let the language flip mid-answer, changing which tokens
/// count from one window to the next.
fn word_offsets(
    text: &str,
    stops: &std::collections::HashSet<&'static str>,
) -> Vec<(usize, String)> {
    use unicode_segmentation::UnicodeSegmentation;
    text.split_word_bound_indices()
        .filter(|(_, w)| w.chars().any(char::is_alphanumeric))
        .map(|(i, w)| (i, w.to_lowercase()))
        .filter(|(_, w)| !stops.contains(w.as_str()))
        .collect()
}

/// One signature per sliding window of the prompt's content words.
///
/// Tokenises through [`word_offsets`], the SAME function the output path uses.
///
/// An earlier cut called `lex_words` here, which segments with
/// `unicode_words()` while the output path uses `split_word_bound_indices()`.
/// On ASCII prose the two agree, and swapping this back is a mutation that
/// survives every test — so this is DEFENSIVE, not a fix for an observed bug,
/// and it would be dishonest to describe it as one. The reason to keep it is
/// that the two segmenters are not specified to agree (contractions,
/// punctuation-adjacent tokens, non-Latin scripts), and a divergence here would
/// not fail loudly: the detector would still trip on a full recitation, just
/// later and weaker. `the_prompt_and_output_paths_tokenise_identically` pins
/// the invariant so a future divergence is caught at the seam rather than
/// showing up as a detector that quietly got worse.
fn window_signatures(text: &str, stops: &std::collections::HashSet<&'static str>) -> Vec<u64> {
    let words: Vec<String> = word_offsets(text, stops)
        .into_iter()
        .map(|(_, w)| w)
        .collect();
    if words.len() < WINDOW_WORDS {
        return Vec::new();
    }
    let hasher = make_simhasher();
    words
        .windows(WINDOW_WORDS)
        .map(|w| hasher.create_signature(w.iter()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic system prompt: long, instructional, repetitive in tone —
    /// exactly the shape that makes false positives easy.
    fn prompt() -> String {
        "You are a coding agent operating inside a user's repository. Always read a file \
         before you edit it, and never guess at a path you have not listed. When you change \
         code you must run the project's tests and report the actual output rather than \
         summarising it. Do not claim that something is verified unless you ran the check \
         yourself in this session. If a tool call fails twice in a row, stop and diagnose \
         the root cause instead of retrying the same call a third time. Prefer the smallest \
         change that solves the problem, and leave the surrounding style alone. Match the \
         comment density and naming conventions of the code around you rather than \
         importing your own. When the user asks a question, answer the question they \
         asked instead of the adjacent one you find more interesting. Never fabricate a \
         file path, a line number, or a command output; if you did not observe it, say \
         so plainly. Keep your final message short enough to read in one screen, and put \
         the diff or the command output below it rather than describing what it contains."
            .to_string()
    }

    fn feed_all(d: &mut PromptLeakDetector, text: &str) -> Option<Leak> {
        d.observe(&format!("{text} "))
    }

    /// A VERBATIM slice of `text` containing exactly `n` content words,
    /// starting at content word `from`.
    ///
    /// Hand-written quotes were calibrated by guessing how many content words a
    /// sentence contains, and the guess was wrong by enough that a fixture
    /// meant to produce five matching windows produced two. Deriving the span
    /// from the same tokeniser the detector uses removes the guess: `n` content
    /// words yield exactly `n - WINDOW_WORDS + 1` matching windows.
    fn verbatim_span(text: &str, from: usize, n: usize) -> &str {
        let stops = crate::llmtrim::stages::tools::stopword_set(text);
        let ws = word_offsets(text, stops);
        assert!(
            from + n <= ws.len(),
            "prompt has {} content words; asked for {}..{}",
            ws.len(),
            from,
            from + n
        );
        let start = ws[from].0;
        let end = ws.get(from + n).map_or(text.len(), |(o, _)| *o);
        text[start..end].trim_end()
    }

    // ---- the acceptance criteria ----

    #[test]
    fn preamble_echo_trips_leak_detector() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt is long enough");
        let leak = feed_all(&mut d, &format!("Sure, here is what I was told. {p}"));
        assert!(leak.is_some(), "reciting the whole prompt did not trip");
    }

    /// THE test in this file. A model quoting one instruction is doing its job.
    #[test]
    fn quoting_a_single_prompt_line_does_not_trip() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let out = "You asked me to always read a file before you edit it, and never guess at \
                   a path you have not listed. I did that: I listed src/, read main.rs, and \
                   the change below is scoped to the one function you named. The tests pass \
                   locally and I have pasted the real output at the end of this message.";
        assert_eq!(
            feed_all(&mut d, out),
            None,
            "quoting one instruction tripped the detector"
        );
    }

    #[test]
    fn normal_output_does_not_trip() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let out = "I looked at the parser and the failure is in the lexer: it treats a \
                   trailing backslash as an escape even at end of input, so the last token \
                   is never emitted. The fix is two lines in scan_string. I ran the suite \
                   and all 412 tests pass. Here is the diff, followed by the test output.";
        assert_eq!(feed_all(&mut d, out), None);
    }

    // ---- the run gate, which is the whole design ----

    /// The discrimination stated directly: identical detector, identical
    /// prompt, and the only difference is HOW MUCH of it came back.
    #[test]
    fn the_trigger_is_run_length_not_mere_similarity() {
        let p = prompt();
        let short = p
            .split_whitespace()
            .take(WINDOW_WORDS + 2)
            .collect::<Vec<_>>()
            .join(" ");
        let mut d1 = PromptLeakDetector::new(&p).expect("prompt");
        assert_eq!(
            feed_all(&mut d1, &short),
            None,
            "a single window's worth of prompt must not trip"
        );
        let mut d2 = PromptLeakDetector::new(&p).expect("prompt");
        assert!(
            feed_all(&mut d2, &p).is_some(),
            "the whole prompt must trip"
        );
    }

    /// A run broken by real output resets. Otherwise a model that quoted one
    /// line per paragraph across a long answer would accumulate to the trip.
    #[test]
    fn an_interrupted_run_does_not_accumulate() {
        let p = prompt();
        let words: Vec<&str> = p.split_whitespace().collect();
        // Three separate short quotes, each under the run threshold, spaced by
        // genuine prose.
        let mut out = String::new();
        for chunk in words.chunks(WINDOW_WORDS + 1).take(3) {
            out.push_str(&chunk.join(" "));
            out.push_str(
                " — anyway, the actual bug was an off-by-one in the ring buffer index and \
                 I have fixed it in the commit below with a regression test. ",
            );
        }
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        assert_eq!(
            feed_all(&mut d, &out),
            None,
            "quotes separated by real prose accumulated into a trip"
        );
    }

    /// The run gate, exercised. Content-word hashing removed the easy false
    /// positives, and with it the proof that the run gate does anything — a
    /// mutation setting TRIP_RUN to 1 survived every other test here, because
    /// none of their fixtures produced even one matching window.
    ///
    /// This quotes the prompt VERBATIM at a length that produces several
    /// matching windows and must still not trip: the case the gate exists for,
    /// a model quoting a couple of its instructions while doing real work.
    #[test]
    fn a_verbatim_quote_below_the_run_threshold_does_not_trip() {
        let p = prompt();
        // Sized to match several windows without reaching the gate. Not an
        // exact count: windows straddling the quote's edges mix in surrounding
        // prose and can still land within the Hamming radius, so the number is
        // asserted as a RANGE below rather than predicted.
        let quote = verbatim_span(&p, 2, WINDOW_WORDS + 2);
        let out = format!(
            "To be explicit about what I was working to: \"{quote}\" That is what I did. \
             The lexer fix is below and the suite passes."
        );
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let verdict = feed_all(&mut d, &out);
        assert_eq!(
            verdict, None,
            "a verbatim quote of that length tripped: {verdict:?}"
        );
        assert!(
            (2..TRIP_RUN).contains(&d.max_run_seen),
            "fixture ran to {} windows; it must match SEVERAL (or it does not exercise the \
             gate) and stay under {TRIP_RUN} (or it is a recitation, not a quote)",
            d.max_run_seen
        );
    }

    /// Verbatim quotes, each under the threshold, separated by real work.
    /// Without the run reset their matches sum past it — and that sum is what a
    /// long answer quoting its instructions twice looks like.
    #[test]
    fn separated_verbatim_quotes_do_not_accumulate() {
        let p = prompt();
        let n = WINDOW_WORDS + 2;
        let q1 = verbatim_span(&p, 0, n);
        let q2 = verbatim_span(&p, n + 2, n);
        let out = format!(
            "You said: \"{q1}\" so I listed src/ first and read the two files I touched. \
             The parser bug was an unterminated escape at end of input; the fix is two \
             lines and a regression test is included. You also said: \"{q2}\" which is why \
             I stopped after the second failure and read the error properly."
        );
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let verdict = feed_all(&mut d, &out);
        assert_eq!(
            verdict, None,
            "separated quotes accumulated into a trip: {verdict:?}"
        );
        assert!(
            (2..TRIP_RUN).contains(&d.max_run_seen),
            "each quote must match several windows and stay under the gate; longest run {}",
            d.max_run_seen
        );
        assert!(
            d.total_matches >= TRIP_RUN,
            "the runs sum to {}, under the {TRIP_RUN} threshold — so the reset is not what \
             is keeping this green and the fixture proves nothing",
            d.total_matches
        );
    }

    /// The escape hatch on the partial-word hold. Without it a stream that
    /// never emits whitespace disables the detector silently, which is the
    /// worst failure mode a guardrail can have.
    #[test]
    fn an_unbroken_token_past_the_cap_is_consumed() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let blob = "x".repeat(5000);
        d.observe(&blob);
        assert_eq!(
            d.consumed,
            blob.len(),
            "a 5000-byte unbroken token was held forever; the detector had stopped looking"
        );
    }

    /// The prompt side and the output side must tokenise IDENTICALLY, or the
    /// comparison is between two different vocabularies.
    ///
    /// Asserted on the SIGNATURES rather than on a run length: the detector
    /// latches at [`TRIP_RUN`], so feeding it the prompt back can never report
    /// a run longer than the gate and could not distinguish "matched
    /// everything" from "matched just enough". That is also how the original
    /// bug hid — it still tripped on full recitations, just later and weaker.
    #[test]
    fn the_prompt_and_output_paths_tokenise_identically() {
        let p = prompt();
        let d = PromptLeakDetector::new(&p).expect("prompt");
        let stops = crate::llmtrim::stages::tools::stopword_set(&p);
        let hasher = make_simhasher();
        let via_output_path: Vec<u64> = word_offsets(&p, stops)
            .into_iter()
            .map(|(_, w)| w)
            .collect::<Vec<_>>()
            .windows(WINDOW_WORDS)
            .map(|w| hasher.create_signature(w.iter()))
            .collect();
        assert_eq!(
            via_output_path.len(),
            d.prompt_sigs.len(),
            "the two paths produced different window counts for the same text"
        );
        assert_eq!(
            via_output_path, d.prompt_sigs,
            "the two paths produced different signatures for the same text"
        );
    }

    // ---- streaming behaviour ----

    /// Fed one delta at a time, the verdict must be the same as fed whole.
    /// A detector that only worked on complete text would be useless on the
    /// path it is built for.
    #[test]
    fn incremental_feeding_matches_whole_text_feeding() {
        let p = prompt();
        let full = format!("Here is what I was told. {p} ");

        let mut whole = PromptLeakDetector::new(&p).expect("prompt");
        let want = whole.observe(&full);
        assert!(want.is_some(), "fixture must trip when fed whole");

        let mut inc = PromptLeakDetector::new(&p).expect("prompt");
        let mut got = None;
        // Cumulative slices at char boundaries, like a real stream.
        let mut cut = 0;
        while cut < full.len() {
            cut = (cut + 7).min(full.len());
            while !full.is_char_boundary(cut) {
                cut += 1;
            }
            if let Some(l) = inc.observe(&full[..cut]) {
                got = Some(l);
                break;
            }
        }
        assert!(got.is_some(), "incremental feeding never tripped");
        assert_eq!(
            got.map(|l| l.start_offset),
            want.map(|l| l.start_offset),
            "incremental and whole-text runs disagreed about where the leak began"
        );
    }

    /// The verdict latches: a caller polling every delta after the trip keeps
    /// getting it, rather than seeing it once and losing it.
    #[test]
    fn the_verdict_latches() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let first = feed_all(&mut d, &p).expect("should trip");
        let again = d.observe(&format!("{p} and now some more text "));
        assert_eq!(again, Some(first));
        assert_eq!(d.leak(), Some(first));
    }

    /// The offset must point at the recitation, not at the start of the
    /// message — the prose before it is the model's real answer and a caller
    /// truncating there would throw it away.
    #[test]
    fn the_offset_points_past_the_real_answer() {
        let p = prompt();
        let preamble = "Here is the fix you asked for, and below it my instructions. ";
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let leak = feed_all(&mut d, &format!("{preamble}{p}")).expect("should trip");
        assert!(
            leak.start_offset >= preamble.len() / 2,
            "leak offset {} landed inside the real answer (preamble is {} bytes)",
            leak.start_offset,
            preamble.len()
        );
    }

    /// A retried turn reuses the detector and starts a shorter cumulative
    /// string. Without the reset the stale run would be built from two
    /// different outputs spliced together.
    #[test]
    fn a_shorter_cumulative_string_resets_rather_than_panicking() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        d.observe("some output from the first attempt that got cut off ");
        assert_eq!(d.observe("short "), None);
        assert_eq!(d.consumed, "short ".len());
    }

    // ---- degenerate inputs ----

    #[test]
    fn a_prompt_too_short_to_window_yields_no_detector() {
        assert!(PromptLeakDetector::new("be nice").is_none());
        assert!(PromptLeakDetector::new("").is_none());
    }

    #[test]
    fn empty_and_whitespace_output_never_trips() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        assert_eq!(d.observe(""), None);
        assert_eq!(d.observe("   \n\t  "), None);
    }

    /// A chunk with no whitespace is held back rather than hashed as a
    /// fragment — but not forever, or a pathological single-token stream would
    /// disable the detector silently.
    #[test]
    fn a_partial_word_is_held_until_its_boundary_arrives() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        assert_eq!(d.observe("alwa"), None);
        assert_eq!(d.consumed, 0, "half a word must not be consumed");
        d.observe("always ");
        assert_eq!(d.consumed, "always ".len());
    }

    #[test]
    fn multibyte_output_does_not_panic_on_slicing() {
        let p = prompt();
        let mut d = PromptLeakDetector::new(&p).expect("prompt");
        let text = "変更を適用しました。テストは全て通っています。 ";
        for _ in 0..4 {
            d.observe(&text.repeat(3));
        }
    }
}
