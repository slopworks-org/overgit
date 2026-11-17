# Overgit

The CLI is defined in `src/main.rs`. Interrupted writes are rolled back in `src/mutation.rs`. The user-facing recovery commands live in `src/recovery.rs`. Integration tests in `tests/` run the compiled CLI against real Git.

## Project conventions

- Keep project Rust code free of `unsafe`. The workspace lints permit documented unsafe blocks.
- Keep the README to a one-screen introduction. Put workflows and safety guidance in command `--help` so users of the installed CLI can find them.
- Changes to CLI data-safety behavior require a real-Git regression test and independent review.

## Definition of done

Adapted from the [Slopworks engineering standard](https://github.com/slopworks-org/.github/blob/main/STANDARD.md#3-definition-of-done).

- **Everything in the repo is written in English**, including code, comments, docs, commit messages, issues and PRs (title and body), whatever language the task was discussed in. The only exception is localization, meaning translations into other languages.
- Keep commits, PR titles and PR bodies free of AI attribution, including `Assisted-by:` or AI `Co-Authored-By:` trailers and generated-with lines.
- Start each task on a fresh branch from `origin/main`. Name it `<type>/<short-task>` using `fix/`, `feat/`, `refactor/` or `chore/`. All changes to `main` go through a PR.
- Keep one task per branch and PR. Split PRs with 300 or more counted lines (`size:XL` or `size:XXL`). The size workflow counts additions and deletions after exclusions such as `tests/` and `Cargo.lock`.
- **The PR title is the squash commit subject.** Follow the conventional commit format checked by `.github/workflows/pr-title.yml`.
  - Type must be `feat`, `fix`, `refactor`, `chore`, `docs`, `test`, `ci`, `perf` or `revert`.
  - Use a scope listed in that workflow. Add the scope for each new module to the list.
  - Subject starts lowercase, uses the imperative and has no trailing period.
- Use the inherited PR template and describe this PR alone. Its body becomes the squash commit body.
- Run `mise run check` before opening the PR. Once it passes, open the PR and report its URL.
- Squash-merge through GitHub.
