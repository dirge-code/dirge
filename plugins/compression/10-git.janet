# Git output compressors — ported from lean-ctx
#
# Functions defined in the shared Janet environment:
#   git-compress [command output] → compressed string or nil

# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

(defn- git-compact-lines [text max]
  (def lines (filter
               (fn [l] (not (empty? (string/trim l))))
               (string/split "\n" text)))
  (if (<= (length lines) max)
    (string/join lines "\n")
    (string (string/join (take max lines) "\n")
            "\n... (" (- (length lines) max) " more lines)")))

(defn- git-extract-change-stats [text]
  (var files 0)
  (var ins 0)
  (var del 0)
  (if-let [c (match "([0-9]+) files? changed" text)]
    (set files (scan-number (in c 0))))
  (if-let [c (match "([0-9]+) insertions?" text)]
    (set ins (scan-number (in c 0))))
  (if-let [c (match "([0-9]+) deletions?" text)]
    (set del (scan-number (in c 0))))
  (if (or (> files 0) (> ins 0) (> del 0))
    (string files " files, +" ins "/-" del)
    ""))

(defn- git-extract-subcommand [command]
  (def tokens (string/split " " command))
  (var seen-git false)
  (var skip-next false)
  (var result nil)
  (each tok tokens
    (if skip-next
      (set skip-next false)
      (let [base (last (string/split "/" tok))]
        (if seen-git
          (if (or (= tok "-C") (= tok "-c") (= tok "--git-dir") (= tok "--work-tree"))
            (set skip-next true)
            (if (not (string/has-prefix? "-" tok))
              (do (set result tok) (break))))
          (if (= base "git")
            (set seen-git true))))))
  result)

(defn- git-diff-or-stat-line? [line]
  (def t (string/trim line))
  (or (string/has-prefix? "diff --git" t)
      (string/has-prefix? "index " t)
      (string/has-prefix? "--- a/" t)
      (string/has-prefix? "+++ b/" t)
      (string/has-prefix? "@@" t)
      (string/has-prefix? "Binary files" t)
      (string/has-prefix? "new file mode" t)
      (string/has-prefix? "deleted file mode" t)
      (string/has-prefix? "old mode" t)
      (string/has-prefix? "new mode" t)
      (string/has-prefix? "similarity index" t)
      (string/has-prefix? "rename from" t)
      (string/has-prefix? "rename to" t)
      (string/has-prefix? "copy from" t)
      (string/has-prefix? "copy to" t)
      (and (string/has-prefix? "+" t) (not (string/has-prefix? "+++" t)))
      (and (string/has-prefix? "-" t) (not (string/has-prefix? "---" t)))
      (and (string/find " | " t)
           (or (string/find "+" t) (string/find "-" t)))))

# ---------------------------------------------------------------------------
# git status
# ---------------------------------------------------------------------------

(defn- git-compress-status [output]
  (var branch "")
  (var ahead 0)
  (var staged @[])
  (var unstaged @[])
  (var untracked @[])
  (var section "")
  (each line (string/split "\n" output)
    (if-let [c (match "On branch ([^ ]+)" line)] (set branch (in c 0)))
    (if-let [c (match "ahead of .+ by ([0-9]+) commit" line)]
      (set ahead (scan-number (in c 0))))
    (if (string/find "Changes to be committed" line) (set section "staged"))
    (if (string/find "Changes not staged" line) (set section "unstaged"))
    (if (string/find "Untracked files" line) (set section "untracked"))
    (def t (string/trim line))
    (if (string/has-prefix? "new file:" t)
      (do
        (def file (string/trim (string/replace "new file:" "" t)))
        (if (= section "staged") (array/push staged (string "+" file)))))
    (if (string/has-prefix? "modified:" t)
      (do
        (def file (string/trim (string/replace "modified:" "" t)))
        (case section
          "staged" (array/push staged (string "~" file))
          "unstaged" (array/push unstaged (string "~" file)))))
    (if (string/has-prefix? "deleted:" t)
      (do
        (def file (string/trim (string/replace "deleted:" "" t)))
        (if (= section "staged") (array/push staged (string "-" file)))))
    (if (string/has-prefix? "renamed:" t)
      (do
        (def file (string/trim (string/replace "renamed:" "" t)))
        (if (= section "staged") (array/push staged (string "\xe2\x86\x92" file)))))  # → (U+2192)
    (if (and (= section "untracked")
             (not (empty? t))
             (not (string/has-prefix? "(" t))
             (not (string/has-prefix? "Untracked" t)))
      (array/push untracked t)))
  (if (and (empty? branch) (empty? staged) (empty? unstaged) (empty? untracked))
    (break (string/trim output)))
  (def parts @[])
  (def ahead-str (if (> ahead 0) (string " \xe2\x86\x91" ahead) ""))  # ↑ (U+2191)
  (array/push parts (string (if (empty? branch) "?" branch) ahead-str))
  (when (not (empty? staged))
    (array/push parts (string "staged: " (string/join staged " "))))
  (when (not (empty? unstaged))
    (array/push parts (string "unstaged: " (string/join unstaged " "))))
  (when (not (empty? untracked))
    (array/push parts (string "untracked: " (string/join untracked " "))))
  (when (and (string/find "nothing to commit" output) (= (length parts) 1))
    (array/push parts "clean"))
  (string/join parts "\n"))

# ---------------------------------------------------------------------------
# git diff
# ---------------------------------------------------------------------------

(defn- git-stat-only? [output]
  (var has-stat false)
  (var result true)
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (if (empty? t) nil)
    (if (and (string/find " | " t) (or (string/find "+" t) (string/find "-" t)))
      (set has-stat true))
    (if (and (string/find "file" t) (string/find "changed" t)) nil)
    (if (or (string/find "insertion" t) (string/find "deletion" t)) nil)
    (if (or (string/has-prefix? "diff --git" t) (string/has-prefix? "@@" t))
      (do (set result false) (break))))
  (and has-stat result))

(defn- git-compress-diff-keep-hunks [output]
  (def result @[])
  (var context-run 0)
  (each line (string/split "\n" output)
    (cond
      (or (string/has-prefix? "diff --git" line) (string/has-prefix? "@@" line))
      (do (set context-run 0) (array/push result line))
      (string/has-prefix? "index " line) nil
      (or (string/has-prefix? "--- " line) (string/has-prefix? "+++ " line))
      (array/push result line)
      (or (string/has-prefix? "+" line) (string/has-prefix? "-" line))
      (do (set context-run 0) (array/push result line))
      (do
        (++ context-run)
        (if (<= context-run 3) (array/push result line)))))
  (if (empty? result) output (string/join result "\n")))

(defn- git-compress-diff [output]
  (if (git-stat-only? output) (break output))
  (def lines (string/split "\n" output))
  (if (<= (length lines) 500) (break (git-compress-diff-keep-hunks output)))
  (def file-ranges @[])
  (var file-start nil)
  (var file-name nil)
  (each [i line] (pairs lines)
    (if (string/has-prefix? "diff --git" line)
      (do
        (when (not (nil? file-start))
          (array/push file-ranges [file-start (dec i) file-name]))
        (set file-start i)
        (def parts (string/split " b/" line))
        (set file-name (if (> (length parts) 1) (in parts 1) "?")))))
  (when (not (nil? file-start))
    (array/push file-ranges [file-start (length lines) file-name]))
  (if (empty? file-ranges) (break (git-compress-diff-keep-hunks output)))
  (def result @[])
  (each [start end name] file-ranges
    (def file-lines (tuple/slice lines start end))
    (if (<= (length file-lines) 250)
      (each l file-lines (array/push result l))
      (do
        (each l (take 200 file-lines) (array/push result l))
        (array/push result (string "[WARNING: diff truncated ("
                                 (- (length file-lines) 250)
                                 " lines hidden)]"))
        (each l (take 50 (drop (- (length file-lines) 50) file-lines))
          (array/push result l)))))
  (string/join result "\n"))

# ---------------------------------------------------------------------------
# Simple compressors: add, branch, checkout, pull, fetch, clone
# ---------------------------------------------------------------------------

(defn- git-compress-add [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" t)))
  (if (<= (length lines) 3) t
    (string "ok (+" (length lines) " files)")))

(defn- git-compress-branch [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (def branches (map
                  (fn [l]
                    (if (string/has-prefix? "*" (string/trim l))
                      (string "*" (string/trim (string/slice (string/trim l) 1)))
                      (string/trim l)))
                  (filter (fn [l] (not (empty? (string/trim l))))
                          (string/split "\n" t))))
  (string/join branches ", "))

(defn- git-compress-checkout [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (each line (string/split "\n" t)
    (def l (string/trim line))
    (if (or (string/has-prefix? "Switched to" l) (string/has-prefix? "Already on" l))
      (break (string "\xe2\x86\x92 "
                     (if-let [b (last (string/split "'" l))] b l)))))
  (git-compact-lines t 3))

(defn- git-compress-pull [output]
  (def t (string/trim output))
  (if (string/find "Already up to date" t) (break "ok (up-to-date)"))
  (def stats (git-extract-change-stats t))
  (if (not (empty? stats)) (break (string "ok " stats)))
  (git-compact-lines t 5))

(defn- git-compress-fetch [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (def new-items @[])
  (each line (string/split "\n" t)
    (when (or (string/find "[new branch]" line) (string/find "[new tag]" line))
      (if-let [name (last (string/split "->" line))]
        (array/push new-items (string/trim name)))))
  (if (empty? new-items) "ok (fetched)"
    (string "ok (new: " (string/join new-items ", ") ")")))

(defn- git-compress-clone [output]
  (var objects 0)
  (each line (string/split "\n" output)
    (if-let [c (match "Receiving objects:[^0-9]*([0-9]+)" line)]
      (set objects (scan-number (in c 0)))))
  (def into (if-let [l (first (filter
                               (fn [x] (string/find "Cloning into" x))
                               (string/split "\n" output)))]
             (if-let [name (last (string/split "'" l))] name "repo")
             "repo"))
  (if (> objects 0)
    (string "cloned '" into "' (" objects " objects)")
    (string "cloned '" into "'")))

# ---------------------------------------------------------------------------
# git merge / tag / reset / remote
# ---------------------------------------------------------------------------

(defn- git-compress-merge [output]
  (def t (string/trim output))
  (if (string/find "Already up to date" t) (break "ok (up-to-date)"))
  (if (string/find "CONFLICT" t)
    (break
      (do
        (def conflicts (filter (fn [l] (string/find "CONFLICT" l))
                               (string/split "\n" t)))
        (string "CONFLICT (" (length conflicts) " files):\n"
                (string/join conflicts "\n")))))
  (def stats (git-extract-change-stats t))
  (if (not (empty? stats)) (string "merged " stats)
    (git-compact-lines t 3)))

(defn- git-compress-tag [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (def tags (filter (fn [l] (not (empty? (string/trim l))))
                    (string/split "\n" t)))
  (if (<= (length tags) 10)
    (string/join tags ", ")
    (string (string/join (take 5 tags) ", ") " (... " (length tags) " total)")))

(defn- git-compress-reset [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (def unstaged (filter
                  (fn [l]
                    (def lt (string/trim l))
                    (or (string/has-prefix? "M" lt)
                        (string/has-prefix? "D" lt)
                        (string/has-prefix? "A" lt)))
                  (string/split "\n" t)))
  (if (empty? unstaged)
    (git-compact-lines t 3)
    (string "reset ok (" (length unstaged) " files unstaged)")))

(defn- git-compress-remote [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (def remotes @{})
  (each line (string/split "\n" t)
    (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
    (when (>= (length parts) 2)
      (put remotes (in parts 0) (in parts 1))))
  (if (empty? remotes) t
    (string/join (map (fn [[k v]] (string k ": " v)) (pairs remotes)) "\n")))

# ---------------------------------------------------------------------------
# git cherry-pick / rebase / bisect
# ---------------------------------------------------------------------------

(defn- git-compress-cherry-pick [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (if (string/find "CONFLICT" t) (break "CONFLICT (cherry-pick)"))
  (def stats (git-extract-change-stats t))
  (if (not (empty? stats)) (string "ok " stats)
    (git-compact-lines t 3)))

(defn- git-compress-rebase [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (if (or (string/find "Already up to date" t) (string/find "is up to date" t))
    (break "ok (up-to-date)"))
  (if (string/find "Successfully rebased" t)
    (do
      (def stats (git-extract-change-stats t))
      (if (empty? stats) (break "ok (rebased)"))
      (break (string "ok (rebased) " stats))))
  (if (string/find "CONFLICT" t)
    (do
      (def conflicts (filter (fn [l] (string/find "CONFLICT" l))
                             (string/split "\n" t)))
      (break (string "CONFLICT (" (length conflicts) " files):\n"
                     (string/join conflicts "\n")))))
  (git-compact-lines t 5))

(defn- git-compress-bisect [output]
  (def t (string/trim output))
  (if (empty? t) (break "ok"))
  (each line (string/split "\n" t)
    (def l (string/trim line))
    (if (string/find "is the first bad commit" l)
      (do
        (def hash (first (string/split " " l)))
        (break (string "found: " (string/slice hash 0 (min 7 (length hash)))
                       " is first bad commit"))))
    (if (string/has-prefix? "Bisecting:" l) (break l)))
  (git-compact-lines t 5))

# ---------------------------------------------------------------------------
# git log
# ---------------------------------------------------------------------------

(defn- git-compress-log [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (when (<= (length lines) 10) (break output))
  (var commits @[])
  (var cur-hash "")
  (var cur-date "")
  (var cur-msg "")
  (each line lines
    (cond
      (string/has-prefix? "commit " line)
      (do
        (when (not (empty? cur-hash))
          (array/push commits (string cur-hash " " cur-date " " cur-msg)))
        (set cur-hash (string/slice (string/trim line) 7 15))
        (set cur-date "")
        (set cur-msg ""))
      (string/has-prefix? "Date: " line)
      (set cur-date (string/trim (string/replace "Date: " "" line)))
      (string/has-prefix? "    " line)
      (set cur-msg (string/trim line))))
  (when (not (empty? cur-hash))
    (array/push commits (string cur-hash " " cur-date " " cur-msg)))
  (def total (length commits))
  (if (> total 30)
    (string (string/join (take 15 commits) "\n")
            "\n... (" (- total 15) " more commits, " total " total)")
    (if (> total 0)
      (string total " commits:\n" (string/join commits "\n"))
      output)))

# ---------------------------------------------------------------------------
# Subcommand dispatch
# ---------------------------------------------------------------------------

(defn git-compress [command output]
  (def sub (git-extract-subcommand command))
  (if (nil? sub) (break nil))
  (def trimmed-output (string/trim output))
  (case sub
    "status" (git-compress-status output)
    "diff" (git-compress-diff output)
    "add" (git-compress-add trimmed-output)
    "branch" (git-compress-branch trimmed-output)
    "checkout" (git-compress-checkout trimmed-output)
    "switch" (git-compress-checkout trimmed-output)
    "pull" (git-compress-pull trimmed-output)
    "fetch" (git-compress-fetch trimmed-output)
    "clone" (git-compress-clone trimmed-output)
    "merge" (git-compress-merge trimmed-output)
    "tag" (git-compress-tag trimmed-output)
    "reset" (git-compress-reset trimmed-output)
    "remote" (if (string/find "remote add" command)
               (git-compress-add trimmed-output)
               (git-compress-remote trimmed-output))
    "cherry-pick" (git-compress-cherry-pick trimmed-output)
    "rebase" (git-compress-rebase trimmed-output)
    "bisect" (git-compress-bisect trimmed-output)
    "log" (git-compress-log output)
    nil))

nil
