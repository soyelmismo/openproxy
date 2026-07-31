---
name: pr-merge
description: Repo-agnostic skill to inspect, analyze, test, review, and merge Pull Requests prioritizing gh CLI.
---

# PR Analysis & Merge Workflow

Automated, repo-agnostic procedure to analyze, review, and merge Pull Requests using `gh` CLI.

---

## 1. List & Select PR
* List open PRs:
  ```bash
  gh pr list
  ```
* View detailed PR metadata:
  ```bash
  gh pr view <PR_NUMBER>
  ```

---

## 2. Verify CI & Automated Checks
* Check CI test suite and status checks:
  ```bash
  gh pr checks <PR_NUMBER>
  ```
* If checks are failing, view logs or fail reasons before proceeding.

---

## 3. Diff Analysis & Code Review
* Fetch full PR diff:
  ```bash
  gh pr diff <PR_NUMBER>
  ```
* Analyze changes for:
  - Security vulnerabilities or leaked credentials
  - Logical bugs or race conditions
  - Breaking API changes or regression risks
  - Adherence to project conventions

---

## 4. Local Checkout & Validation (Optional / Critical Changes)
* Checkout PR branch locally if local execution or testing is required:
  ```bash
  gh pr checkout <PR_NUMBER>
  ```
* Execute project test runner (e.g., `cargo test`, `npm test`, `pytest`, etc.).

---

## 5. Review & Merge
* **If PR is approved and checks pass:**
  Merge PR and cleanup head branch:
  ```bash
  gh pr merge <PR_NUMBER> --squash --delete-branch
  ```
  *(Use `--rebase` or `--merge` if repository workflow requires explicit merge strategy)*

* **If PR requires changes:**
  Submit review with feedback:
  ```bash
  gh pr review <PR_NUMBER> --request-changes --body "<FEEDBACK_SUMMARY>"
  ```

* **If posting approval without auto-merge:**
  ```bash
  gh pr review <PR_NUMBER> --approve --body "<APPROVAL_SUMMARY>"
  ```
