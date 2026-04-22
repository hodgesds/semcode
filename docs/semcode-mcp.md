# semcode usage guide

All semcode functions are git aware and default to lookups on the current
commit.  You can also pass a specific commit you're interested in, or a branch name.

**Regex**: all patterns are case-insensitive; no `(?i)` needed.  Applies to
function names, commit messages, symbols, and lore email searches.

## Common parameters

- **git_sha**: commit to search (default: current)
- **branch**: branch name, resolved to its tip (e.g., "main"); takes
  precedence over git_sha if both are given
- **page**: pagination (1-based), 50 lines per page; omit for full results
- **since_date / until_date**: e.g., "yesterday", "2 weeks ago",
  "2024-01-15"
- **\*_patterns**: arrays of regex.  `author_patterns`, `subject_patterns`,
  `from_patterns`, `body_patterns`, `recipients_patterns`,
  `symbols_patterns`, `path_patterns` are OR'd within an array.
  `regex_patterns` and `symbol_patterns` are AND'd within an array.

**Conventions**: boolean parameters default to `false`; `limit: 0`
means unlimited unless a max is given.

## Code lookup

**find_function**: search for functions and macros
  - name: function/macro name, or a regex
  - also displays details on callers and callees
**find_type**: search for types and typedefs
  - name: type/typedef name or regex
**find_callers**: find all functions that call a function or macro
  - name: function to search
**find_calls**: find all functions called by a function or macro
  - name: function to search
**find_callchain**: search complete function/macro call chain (forward and reverse)
  - name: function or macro to search
  - up_levels: number of caller levels to show (default: 2, 0 = unlimited)
  - down_levels: number of callee levels to show (default: 3, 0 = unlimited)
  - calls_limit: max calls to show per level (default: 15, 0 = unlimited)
**diff_functions**: extract functions and types from a unified diff
  - diff_content: the string to analyze
  - Use this to determine which symbols are involved in a given diff

## Code search

**grep_functions**: search function/macro bodies for a regex
  - pattern: the regex to search for
  - verbose: if true, show full function bodies
  - path_pattern: optional regex to filter results by path
  - limit: max number of results (default: 100)
  - only searches inside functions or macros; no need to escape
    your pattern to limit the search
**vgrep_functions**: vector embedding search on functions/macros/types
  - query_text: text describing the kind of functions to find
  - path_pattern: optional regex to filter results by path
  - limit: max number of results (default: 10, max: 100)
  - only useful for broad concepts that a regex won't find well
  - the database might not have embeddings indexed

## Commit search

In both tools below, `reachable_sha` and `git_range` are mutually
exclusive.  To search commits reachable from HEAD, pass
`reachable_sha=HEAD` alone.

**find_commit**: search for changes, potentially in a range of commits
  - can return a large body of results; use pagination to manage context
  - git_ref: single commit ref (sha, short sha, branch, HEAD, etc.)
  - git_range: optional range for multiple commits, e.g., HEAD~10..HEAD;
    cannot be combined with git_ref
  - reachable_sha: optional git sha; filter to results reachable from it
  - regex_patterns: applied against commit message + unified diff
  - symbol_patterns: find commits changing a function or type
  - verbose: show full diff in addition to metadata
  - accepts: author_patterns, subject_patterns, path_patterns
**vcommit_similar_commits**: search commits based on vector embeddings
  - query_text: search text
  - git_range: optional range, e.g., HEAD~10..HEAD
  - reachable_sha: optional git sha, reachable-from filter
  - regex_patterns: AND'd to limit results
  - symbol_patterns: AND'd to limit results by symbols changed
  - limit: max results (default 10, max 50)
  - accepts: author_patterns, subject_patterns, path_patterns

## Lore (kernel mailing list archive)

**lore_search**: search lore.kernel.org email archives
  - message_id: optional exact message ID for direct lookup
  - verbose: show full message body
  - show_thread: show full email thread for each match
  - show_replies: show replies/subthreads under each match
    (mutually exclusive with show_thread)
  - mbox: output in MBOX format with full headers and body
  - limit: max number of results (default: 100)
  - accepts: from_patterns, subject_patterns, body_patterns,
    symbols_patterns, recipients_patterns
**dig**: find lore.kernel.org emails related to a git commit
  - commit: git commit reference (SHA, short SHA, HEAD, branch name, etc.)
  - verbose: show full message body
  - show_all: show all duplicate results, not just most recent
  - show_thread: show full thread for each result (use with show_all)
  - show_replies: show replies/subthreads (use with show_all, mutually
    exclusive with show_thread)
**vlore_similar_emails**: semantic vector search over lore.kernel.org emails
  - query_text: text describing the kind of emails to find
  - limit: max number of results (default: 20, max: 100)
  - accepts: from_patterns, subject_patterns, body_patterns,
    symbols_patterns, recipients_patterns
  - the database might not have lore embeddings indexed

## Branch / status

**list_branches**: list indexed branches with indexed SHA and
  freshness (up-to-date vs. outdated against current tip).  No
  parameters.
**compare_branches**: compare two branches; shows merge base,
  ahead/behind status, and indexing status for both
  - branch1, branch2: branch names
**indexing_status**: show background indexing progress, errors,
  and timing.  No parameters.

## Lazy Loading

Start the server with `--lazy` to cut initial context ~96%.  The
server then exposes only three meta-tools (`list_categories`,
`get_tools`, `call_tool`); call them in that order to discover
and invoke full tools on demand.

## Recipes

### Searching for commits reachable from HEAD (or any other git sha)

If a repository heavily cherry-picks patches, it might have a backported commit
under a different git sha.  This means the most effective way to find the
backported commit is searching by commit subject:

```
semcode> commit -r "bnxt_en: Fix memory corruption when FW resources change during ifdown"
semcode> commit -r "bnxt_en: Fix memory corruption when FW resources change during ifdown" --reachable HEAD
```
