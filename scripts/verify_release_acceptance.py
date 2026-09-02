#!/usr/bin/env python3
"""Policy verifier for canonical, SHA-bound provider-canary evidence."""
import datetime as dt
import hashlib
import json
import pathlib
import re
import sys
from verify_json_record import InvalidRecord, load_record

H40 = re.compile(r"[0-9a-f]{40}$")
H64 = re.compile(r"[0-9a-f]{64}$")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}$")
IDENT = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/@-]{0,127}$")
TIME = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
PROFILES = {
    "deepgram-batch": (("batch", "deepgram", "nova-3", 2),),
    "elevenlabs-batch": (("batch", "elevenlabs", "scribe_v2", 3),),
    "deepgram-live": (("live", "deepgram", "nova-3", 2),),
    "elevenlabs-live": (("live", "elevenlabs", "scribe_v2_realtime", 2),),
    "deepgram-batch-elevenlabs-live": (("batch", "deepgram", "nova-3", 2), ("live", "elevenlabs", "scribe_v2_realtime", 2)),
    "elevenlabs-batch-deepgram-live": (("batch", "elevenlabs", "scribe_v2", 3), ("live", "deepgram", "nova-3", 2)),
}
BASE = {"position", "provider", "model", "mode", "contract_version", "language", "fixture_id", "fixture_digest", "result_digest", "effect_id", "provider_request_id", "provider_result_id", "started_at", "completed_at", "latency_ms", "outcome", "error_classification"}
BOUND = ("fixture_id", "fixture_digest", "result_digest", "effect_id", "provider_request_id", "provider_result_id")


def fail(message):
    raise InvalidRecord(message)


def exact(value, keys, name):
    if not isinstance(value, dict) or set(value) != set(keys):
        fail(f"{name} has missing or unknown keys")
    return value


def text(value, pattern, name):
    if not isinstance(value, str) or not pattern.fullmatch(value):
        fail(f"invalid {name}")
    return value


def integer(value, low, high, name):
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        fail(f"invalid {name}")
    return value


def timestamp(value, name):
    value = text(value, TIME, name)
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=dt.timezone.utc)
    except ValueError as error:
        raise InvalidRecord(f"invalid {name}") from error


def file_sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def canonical(path, value):
    expected = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if path.read_bytes() != expected:
        fail(f"{path.name} is not canonical JSON")


def runner(value, name):
    value = exact(value, {"identity", "revision"}, name)
    text(value["identity"], IDENT, f"{name}.identity")
    text(value["revision"], H40, f"{name}.revision")
    return value


def fixture_manifest(value):
    value = exact(value, {"schema_version", "campaign_id", "fixtures"}, "fixture manifest")
    if value["schema_version"] != 1 or not isinstance(value["fixtures"], list):
        fail("invalid fixture manifest")
    text(value["campaign_id"], IDENT, "fixture campaign_id")
    fixtures = {}
    for item in value["fixtures"]:
        item = exact(item, {"fixture_id", "sha256"}, "fixture")
        key = text(item["fixture_id"], IDENT, "fixture_id")
        if key in fixtures:
            fail("duplicate fixture_id")
        fixtures[key] = text(item["sha256"], DIGEST, "fixture digest")
    if not fixtures:
        fail("empty fixture manifest")
    return fixtures


def campaign_manifest(value, fixtures):
    value = exact(value, {"schema_version", "source_sha", "image_digest", "campaign_id", "runner", "effects"}, "campaign manifest")
    if value["schema_version"] != 1 or not isinstance(value["effects"], list):
        fail("invalid campaign manifest")
    text(value["source_sha"], H40, "campaign source_sha")
    text(value["image_digest"], DIGEST, "campaign image_digest")
    text(value["campaign_id"], IDENT, "campaign_id")
    runner(value["runner"], "campaign runner")
    effects, identifiers = {}, set()
    keys = {"position", "profile", "kind", "fixture_id", "fixture_digest", "result_digest", "effect_id", "provider_request_id", "provider_result_id"}
    for item in value["effects"]:
        item = exact(item, keys, "campaign effect")
        position = text(item["position"], IDENT, "effect position")
        if position in effects:
            fail("duplicate effect position")
        if item["profile"] not in PROFILES or item["kind"] not in ("batch", "live"):
            fail("invalid effect profile or kind")
        fixture_id = text(item["fixture_id"], IDENT, "effect fixture_id")
        fixture_digest = text(item["fixture_digest"], DIGEST, "effect fixture digest")
        if fixtures.get(fixture_id) != fixture_digest:
            fail("fixture digest is not bound by fixture manifest")
        text(item["result_digest"], DIGEST, "result digest")
        for field in ("effect_id", "provider_request_id", "provider_result_id"):
            identifier = text(item[field], IDENT, field)
            if identifier in identifiers:
                fail("provider request/result/effect IDs are not globally unique")
            identifiers.add(identifier)
        effects[position] = item
    if len(effects) != 8:
        fail("campaign manifest must contain exactly eight fresh effects")
    return effects


def effect(value, expected, profile, effects, campaign_end):
    kind, provider, model, version = expected
    extra = {"provider_terminal"} if kind == "batch" else {"accepted_frame_count", "accepted_frames_digest", "ack_first", "ack_last", "finalize"}
    value = exact(value, BASE | extra, f"{profile} {kind}")
    position = text(value["position"], IDENT, "position")
    if (value["provider"], value["model"], value["mode"], value["contract_version"], value["language"]) != (provider, model, kind, version, "multi"):
        fail(f"wrong provider identity at {position}")
    start, end = timestamp(value["started_at"], "started_at"), timestamp(value["completed_at"], "completed_at")
    latency = integer(value["latency_ms"], 0, 86_400_000, "latency_ms")
    if end < start or end > campaign_end or int((end - start).total_seconds() * 1000) != latency:
        fail(f"invalid timestamps at {position}")
    if value["outcome"] != "pass" or value["error_classification"] != "none":
        fail(f"non-passing effect at {position}")
    bound = effects.get(position)
    if not bound or bound["profile"] != profile or bound["kind"] != kind:
        fail(f"effect position is not campaign-bound: {position}")
    if any(value[field] != bound[field] for field in BOUND):
        fail(f"effect differs from campaign manifest at {position}")
    if kind == "batch":
        terminal = exact(value["provider_terminal"], {"status", "provider_result_id", "observed_at"}, "batch terminal")
        observed = timestamp(terminal["observed_at"], "batch terminal observed_at")
        if terminal["status"] != "completed" or terminal["provider_result_id"] != value["provider_result_id"] or not start <= observed <= end:
            fail("batch terminal is not linked to provider result")
    else:
        count = integer(value["accepted_frame_count"], 1, 1_000_000, "accepted_frame_count")
        if integer(value["ack_first"], 1, 1_000_000, "ack_first") != 1 or integer(value["ack_last"], 1, 1_000_000, "ack_last") != count:
            fail("live ACK range does not cover every accepted frame")
        text(value["accepted_frames_digest"], DIGEST, "accepted frames digest")
        finalize = exact(value["finalize"], {"status", "provider_result_id", "terminal_at"}, "finalize")
        terminal = timestamp(finalize["terminal_at"], "finalize terminal_at")
        if finalize["status"] != "flushed" or finalize["provider_result_id"] != value["provider_result_id"] or not start <= terminal <= end:
            fail("live finalize is not linked to provider result")
    return position


def canary_record(value, effects, source_sha, image_digest):
    keys = {"schema_version", "source_sha", "image_digest", "campaign_id", "runner", "credential_owner", "campaign_manifest_sha256", "fixture_manifest_sha256", "result", "completed_at", "checks"}
    value = exact(value, keys, "provider canary")
    if value["schema_version"] != 1 or value["source_sha"] != source_sha or value["image_digest"] != image_digest:
        fail("provider canary release identity mismatch")
    text(value["campaign_id"], IDENT, "canary campaign_id")
    runner(value["runner"], "canary runner")
    text(value["credential_owner"], IDENT, "credential_owner")
    text(value["campaign_manifest_sha256"], H64, "campaign manifest SHA")
    text(value["fixture_manifest_sha256"], H64, "fixture manifest SHA")
    completed = timestamp(value["completed_at"], "canary completed_at")
    if value["result"] != "pass" or not isinstance(value["checks"], list) or len(value["checks"]) != 6:
        fail("provider canary is not a complete pass")
    profiles, positions = set(), set()
    for check in value["checks"]:
        check = exact(check, {"profile", "result", "batch", "live"}, "canary check")
        profile = check["profile"]
        if profile not in PROFILES or profile in profiles or check["result"] != "pass":
            fail("invalid or duplicate canary profile")
        profiles.add(profile)
        expected = PROFILES[profile]
        for part in expected:
            kind = part[0]
            if check[kind] is None:
                fail(f"missing {kind} effect for {profile}")
            position = effect(check[kind], part, profile, effects, completed)
            if position in positions:
                fail("standalone or mixed effect was reused")
            positions.add(position)
        for kind in {"batch", "live"} - {part[0] for part in expected}:
            if check[kind] is not None:
                fail(f"unexpected {kind} effect for {profile}")
    if profiles != set(PROFILES) or positions != set(effects):
        fail("canary effects do not exactly cover campaign manifest")
    return value


def approval_record(value, source_sha, image_digest):
    keys = {"schema_version", "source_sha", "image_digest", "campaign_id", "decision", "authorization", "protected_environment", "approval_workflow_revision", "workflow_run_id", "approved_at", "runner", "canary_payload_sha256", "campaign_manifest_sha256", "fixture_manifest_sha256", "trust_policy_sha256"}
    value = exact(value, keys, "reviewer approval")
    if value["schema_version"] != 1 or value["source_sha"] != source_sha or value["image_digest"] != image_digest:
        fail("approval release identity mismatch")
    if value["decision"] != "approved" or value["authorization"] != "github-environment-required-reviewer":
        fail("approval authorization is invalid")
    for field in ("campaign_id", "protected_environment"):
        text(value[field], IDENT, field)
    text(value["approval_workflow_revision"], H40, "approval workflow revision")
    text(value["workflow_run_id"], re.compile(r"[1-9][0-9]{0,19}$"), "workflow run ID")
    timestamp(value["approved_at"], "approved_at")
    runner(value["runner"], "approval runner")
    for field in ("canary_payload_sha256", "campaign_manifest_sha256", "fixture_manifest_sha256", "trust_policy_sha256"):
        text(value[field], H64, field)
    return value


def trust_policy(value):
    value = exact(value, {"schema_version", "approval_attestation", "canary_runner"}, "trust policy")
    if value["schema_version"] != 1:
        fail("invalid trust policy version")
    attestation = exact(value["approval_attestation"], {"repository", "signer_workflow", "predicate_type", "protected_environment"}, "approval attestation policy")
    for field in attestation:
        text(attestation[field], IDENT, f"approval attestation {field}")
    trusted_runner = exact(value["canary_runner"], {"identity"}, "canary runner policy")
    text(trusted_runner["identity"], IDENT, "trusted canary runner identity")
    return value


def main():
    if len(sys.argv) != 4:
        print(f"usage: {sys.argv[0]} SOURCE_SHA IMAGE_DIGEST EVIDENCE_DIRECTORY", file=sys.stderr)
        return 2
    source_sha, image_digest, directory = sys.argv[1:]
    if not H40.fullmatch(source_sha) or not DIGEST.fullmatch(image_digest):
        return 2
    root, base = pathlib.Path.cwd(), pathlib.Path(directory)
    paths = {"approval": base / "acceptance/reviewer-approval.json", "canary": base / "acceptance/provider-canary.json", "campaign": base / "acceptance/campaign-manifest.json", "fixtures": base / "acceptance/fixture-manifest.json", "policy": root / "security/release-trust-policy.json"}
    try:
        values = {name: load_record(path) for name, path in paths.items()}
        for name in ("approval", "canary", "campaign", "fixtures"):
            canonical(paths[name], values[name])
        policy = trust_policy(values["policy"])
        fixtures = fixture_manifest(values["fixtures"])
        effects = campaign_manifest(values["campaign"], fixtures)
        canary = canary_record(values["canary"], effects, source_sha, image_digest)
        approval = approval_record(values["approval"], source_sha, image_digest)
        campaign = values["campaign"]
        if campaign["source_sha"] != source_sha or campaign["image_digest"] != image_digest:
            fail("campaign release identity mismatch")
        if campaign["campaign_id"] != canary["campaign_id"] or values["fixtures"]["campaign_id"] != canary["campaign_id"] or campaign["runner"] != canary["runner"]:
            fail("campaign or runner mismatch")
        if canary["runner"]["identity"] != policy["canary_runner"]["identity"]:
            fail("canary runner is not trusted by repository policy")
        expected = {"canary_payload_sha256": file_sha(paths["canary"]), "campaign_manifest_sha256": file_sha(paths["campaign"]), "fixture_manifest_sha256": file_sha(paths["fixtures"]), "trust_policy_sha256": file_sha(paths["policy"])}
        if canary["campaign_manifest_sha256"] != expected["campaign_manifest_sha256"] or canary["fixture_manifest_sha256"] != expected["fixture_manifest_sha256"]:
            fail("canary manifest binding mismatch")
        if approval["protected_environment"] != policy["approval_attestation"]["protected_environment"] or approval["approval_workflow_revision"] != source_sha:
            fail("approval environment or workflow revision is not trusted")
        if approval["campaign_id"] != canary["campaign_id"] or approval["runner"] != canary["runner"] or any(approval[key] != value for key, value in expected.items()):
            fail("approval SHA-256, campaign, or runner binding mismatch")
    except (InvalidRecord, OSError, UnicodeError) as error:
        print(f"invalid release acceptance evidence: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
