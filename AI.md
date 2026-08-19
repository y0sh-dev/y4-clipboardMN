

## Generative AI (LLM) Usage Policy and Guidelines
This project does not restrict or prohibit the use of Generative AI (LLMs) for complex problem-solving or refactoring. While AI is a powerful tool, we share the following common understandings to ensure that we, as contributors, approach the codebase with integrity and care.
## Core Philosophy

* Technical Understanding: When addressing logical flaws or bugs discovered by an AI, you must personally and technically comprehend why it constitutes an issue, and be capable of verifying whether the proposed fix is correct.
* Reviewing the Current Repository: Before proceeding with a fix, please ensure you check the current status of the repository to confirm that the issue hasn't already been resolved or addressed in an existing discussion.
* Careful Consideration: When adopting AI-generated suggestions, please exercise thorough review and consideration, keeping in mind the context of the existing code and its potential impact on other developers.

## Branching and Commit Guidelines
To keep the development process transparent and easy to follow, please separate any AI-assisted changes into a dedicated branch.

* Branch Naming Convention: ai/your-branch-text (e.g., ai/fix-logic-error so that the AI involvement is clear at a glance).
* Commit Messages: We highly recommend including the specific LLM model used (e.g., Claude 3.5 Sonnet) within your commit messages. Omitting this information makes it difficult for fellow contributors to trace the context and rationale behind the change, which may affect the status of the Pull Request (such as leading to a closure).

## Pull Request (PR) Guidelines
When opening a Pull Request that involves AI-assisted modifications, please consider your fellow reviewers by adhering to the following requirements:

* Summary of Changes: Have the AI generate a summary explaining what the issue was and how it was resolved, and include this description in your PR.
* Clarity of Intent: To maintain the overall quality of the project, if the intent or rationale behind a PR is ambiguous, or if human verification appears insufficient, the PR may be closed or cancelled.
