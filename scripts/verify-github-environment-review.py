#!/usr/bin/env python3
"""Fail closed unless this first-attempt run has a protected environment review."""

import json
import subprocess
import sys


EXPECTED_REPOSITORY = "777genius/voicetext-gateway"
MAX_RESPONSE_BYTES = 1024 * 1024


class ReviewError(ValueError):
    pass


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ReviewError(f"duplicate object key: {key}")
        value[key] = item
    return value


def positive_id(value, label):
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ReviewError(f"{label} must be a positive integer")
    return value


def read_api(path):
    try:
        result = subprocess.run(
            [
                "gh",
                "api",
                "--method",
                "GET",
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2022-11-28",
                path,
            ],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise ReviewError(f"GitHub API request failed for {path}") from error
    if not result.stdout or len(result.stdout) > MAX_RESPONSE_BYTES:
        raise ReviewError(f"GitHub API response is empty or oversized for {path}")
    try:
        text = result.stdout.decode("utf-8")
        decoder = json.JSONDecoder(object_pairs_hook=unique_object)
        value, end = decoder.raw_decode(text)
    except (UnicodeError, json.JSONDecodeError, ReviewError) as error:
        raise ReviewError(f"GitHub API response is not strict JSON for {path}: {error}") from error
    if text[end:].strip():
        raise ReviewError(f"GitHub API response has trailing JSON data for {path}")
    return value


def verify_configuration(configuration, expected_environment):
    if not isinstance(configuration, dict):
        raise ReviewError("environment response must be an object")
    if configuration.get("name") != expected_environment:
        raise ReviewError("environment response has the wrong name")
    environment_id = positive_id(configuration.get("id"), "environment id")
    rules = configuration.get("protection_rules")
    if not isinstance(rules, list):
        raise ReviewError("environment protection_rules must be an array")
    required = [rule for rule in rules if isinstance(rule, dict) and rule.get("type") == "required_reviewers"]
    if len(required) != 1:
        raise ReviewError("environment must have exactly one required_reviewers protection rule")
    rule = required[0]
    if rule.get("prevent_self_review") is not True:
        raise ReviewError("required_reviewers must set prevent_self_review=true")
    reviewers = rule.get("reviewers")
    if not isinstance(reviewers, list) or not reviewers:
        raise ReviewError("required_reviewers must contain at least one reviewer")
    reviewer_ids = set()
    for reviewer in reviewers:
        if not isinstance(reviewer, dict) or reviewer.get("type") not in ("User", "Team"):
            raise ReviewError("configured reviewers must be GitHub users or teams")
        identity = reviewer.get("reviewer")
        if not isinstance(identity, dict):
            raise ReviewError("configured reviewer identity is missing")
        reviewer_id = positive_id(identity.get("id"), "configured reviewer id")
        if reviewer_id in reviewer_ids:
            raise ReviewError("configured reviewer identities must be unique")
        reviewer_ids.add(reviewer_id)
    return environment_id


def verify_review_history(history, expected_environment, environment_id, actor, actor_id):
    if not isinstance(history, list) or not history:
        raise ReviewError("workflow-run review history must be a nonempty array")
    matching = []
    for review in history:
        if not isinstance(review, dict):
            raise ReviewError("workflow-run review entry must be an object")
        environments = review.get("environments")
        if not isinstance(environments, list):
            raise ReviewError("workflow-run review environments must be an array")
        matched_entry = False
        for environment in environments:
            if not isinstance(environment, dict):
                raise ReviewError("reviewed environment must be an object")
            name = environment.get("name")
            reviewed_id = environment.get("id")
            if name == expected_environment or reviewed_id == environment_id:
                if name != expected_environment or reviewed_id != environment_id:
                    raise ReviewError("review evidence has a mismatched environment name or id")
                if matched_entry:
                    raise ReviewError("review entry repeats the expected environment")
                matched_entry = True
        if matched_entry:
            matching.append(review)
    if len(matching) != 1:
        raise ReviewError("review history must contain exactly one unambiguous review for this environment")
    review = matching[0]
    if review.get("state") != "approved":
        raise ReviewError("environment review state is not approved")
    user = review.get("user")
    if not isinstance(user, dict) or user.get("type") != "User":
        raise ReviewError("environment approver must be a human GitHub user")
    login = user.get("login")
    if not isinstance(login, str) or not login or login.lower().endswith("[bot]"):
        raise ReviewError("environment approver login is missing or is a bot")
    approver_id = positive_id(user.get("id"), "environment approver id")
    if login.casefold() == actor.casefold() or approver_id == actor_id:
        raise ReviewError("environment approval must be independent of the workflow actor")


def main():
    if len(sys.argv) != 7:
        print(
            f"usage: {sys.argv[0]} REPOSITORY ENVIRONMENT RUN_ID RUN_ATTEMPT ACTOR ACTOR_ID",
            file=sys.stderr,
        )
        return 2
    repository, environment, run_id, run_attempt, actor, raw_actor_id = sys.argv[1:]
    try:
        if repository != EXPECTED_REPOSITORY:
            raise ReviewError(f"repository must be exactly {EXPECTED_REPOSITORY}")
        if environment not in ("canary-approval", "release-publication"):
            raise ReviewError("unexpected protected environment")
        if not run_id.isascii() or not run_id.isdigit() or int(run_id) <= 0:
            raise ReviewError("workflow run id must be a positive integer")
        if run_attempt != "1":
            raise ReviewError(
                "reruns are refused: GitHub's review-history API is run-scoped and cannot prove this attempt"
            )
        if not actor:
            raise ReviewError("workflow actor is missing")
        if not raw_actor_id.isascii() or not raw_actor_id.isdigit():
            raise ReviewError("workflow actor id must be a positive integer")
        actor_id = positive_id(int(raw_actor_id), "workflow actor id")
        prefix = f"/repos/{EXPECTED_REPOSITORY}"
        configuration = read_api(f"{prefix}/environments/{environment}")
        environment_id = verify_configuration(configuration, environment)
        history = read_api(f"{prefix}/actions/runs/{run_id}/approvals")
        verify_review_history(history, environment, environment_id, actor, actor_id)
    except ReviewError as error:
        print(f"environment review authorization refused: {error}", file=sys.stderr)
        return 1
    print(f"verified independent review for {repository} environment {environment} run {run_id}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
