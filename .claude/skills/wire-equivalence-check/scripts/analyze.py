#!/usr/bin/env python3
"""Exhaustive wire-equivalence analyzer.

Walks two mitmproxy flow captures (native + forge) and diffs everything
observable on the wire. Produces a structured verdict listing every
finding — known classification leaks, header-set differences, query-
param differences, JSON-key-path differences in request bodies,
response status divergences, telemetry event-type differences, and a
defensive scan for any string pattern that smells like a leak.

Designed for iterative tightening: run, fix the highest-priority
finding, re-run. Each invocation writes a timestamped audit JSON so
consecutive runs can be compared to see "fixed since last run" vs
"newly introduced".

Usage:
    analyze.py --native /tmp/forge-wire-check/flows-native.mitm \
               --alt    /tmp/forge-wire-check/flows-alt.mitm \
               --accepted .claude/skills/wire-equivalence-check/accepted-divergences.json \
               [--audit-dir audits/wire-equivalence] \
               [--verbose]

Exits:
    0  if PASS (zero unaccepted findings)
    1  if FAIL (any unaccepted finding)
    2  if capture missing / unparseable
"""
import argparse, json, sys, re, urllib.parse, os, time, hashlib
from collections import Counter, defaultdict

try:
    from mitmproxy import io as mitm_io
except ImportError:
    print("ERROR: mitmproxy Python lib missing.", file=sys.stderr)
    print("Install with: pip3 install --break-system-packages mitmproxy", file=sys.stderr)
    sys.exit(2)


# ────────────────────────────────────────────────────────────────────
# Survey: walk a capture and extract every observable signal
# ────────────────────────────────────────────────────────────────────

def survey(path):
    if not os.path.exists(path):
        return None

    s = {
        'path': path,
        'flows': 0,
        'sessions': set(),
        'endpoints': Counter(),                       # "METHOD host/path" -> count
        'query_params_per_endpoint': defaultdict(lambda: defaultdict(set)),  # ep -> qparam -> set(values)
        'headers_per_endpoint': defaultdict(lambda: defaultdict(set)),       # ep -> header_name -> set(values)
        'request_body_paths_per_endpoint': defaultdict(set),                 # ep -> set(json-paths-with-types)
        'request_body_sizes_per_endpoint': defaultdict(list),                # ep -> [sizes]
        'response_status_per_endpoint': defaultdict(Counter),                # ep -> status -> count
        'response_content_types_per_endpoint': defaultdict(Counter),         # ep -> ct -> count
        # Classification-specific
        'bootstrap_qs': Counter(),
        'uas_per_endpoint': defaultdict(Counter),
        'event_log_classification': Counter(),
        'datadog_classification': Counter(),
        'event_log_event_names': Counter(),
        'datadog_event_names': Counter(),
        'agent_sdk_version_sightings': Counter(),
        'anthropic_beta_values': Counter(),
        'cc_entrypoint_in_system_prompt': Counter(),
        'msgs': 0,
        'count_tokens_hits': 0,
        'metrics_enabled_hits': 0,
        # Defensive scan
        'suspicious_strings': Counter(),                                     # what+where
        # Body fingerprints
        'body_fingerprint_per_endpoint': defaultdict(set),                   # ep -> set(structural-fingerprints)
    }

    with open(path, 'rb') as f:
        for flow in mitm_io.FlowReader(f).stream():
            if flow.type != 'http':
                continue
            s['flows'] += 1
            req = flow.request
            resp = flow.response
            host = req.host
            url = urllib.parse.urlparse(req.pretty_url)
            ep = f"{req.method} {host}{url.path}"
            s['endpoints'][ep] += 1

            if '/count_tokens' in url.path:
                s['count_tokens_hits'] += 1
            if '/metrics_enabled' in url.path:
                s['metrics_enabled_hits'] += 1

            # Query params
            for k, vs in urllib.parse.parse_qs(url.query).items():
                for v in vs:
                    s['query_params_per_endpoint'][ep][k].add(v)

            # Headers (capture all, except a few known per-request unique ones)
            for hdr_k, hdr_v in req.headers.items(multi=True):
                k = hdr_k.lower()
                # Skip per-request volatile headers
                if k in ('host', 'content-length', 'connection', 'content-encoding',
                         'accept-encoding', 'transfer-encoding',
                         'x-request-id', 'x-claude-code-session-id', 'cookie',
                         'authorization', 'dd-api-key'):
                    continue
                s['headers_per_endpoint'][ep][k].add(hdr_v[:200])  # truncate huge values
                if k == 'user-agent' and 'claude' in hdr_v.lower():
                    s['uas_per_endpoint'][ep][hdr_v] += 1
                if k == 'x-claude-code-session-id' or hdr_k.lower() == 'x-claude-code-session-id':
                    s['sessions'].add(hdr_v)
                if k == 'anthropic-beta':
                    s['anthropic_beta_values'][hdr_v] += 1

            # session-id needs special-case after the filter
            for hdr_k, hdr_v in req.headers.items(multi=True):
                if hdr_k.lower() == 'x-claude-code-session-id':
                    s['sessions'].add(hdr_v)

            if '/bootstrap' in url.path:
                qs = urllib.parse.parse_qs(url.query)
                if 'entrypoint' in qs:
                    s['bootstrap_qs'][qs['entrypoint'][0]] += 1

            if url.path == '/v1/messages':
                s['msgs'] += 1

            # Request body — size, path inventory, classification scan
            if req.content:
                s['request_body_sizes_per_endpoint'][ep].append(len(req.content))
                ct = req.headers.get('content-type', '')
                if 'json' in ct.lower():
                    try:
                        body = json.loads(req.content)
                    except Exception:
                        body = None
                    if body is not None:
                        # Walk all paths in the body, recording type at each
                        for path_with_type in walk_paths(body):
                            s['request_body_paths_per_endpoint'][ep].add(path_with_type)
                        # Structural fingerprint for repeated endpoints
                        fp = fingerprint(body)
                        s['body_fingerprint_per_endpoint'][ep].add(fp)

                        # cc_entrypoint=<X> in /v1/messages system prompt
                        if url.path == '/v1/messages' and isinstance(body, dict):
                            sys_arr = body.get('system')
                            if isinstance(sys_arr, list):
                                for entry in sys_arr:
                                    text = entry.get('text', '') if isinstance(entry, dict) else ''
                                    for m in re.finditer(r'cc_entrypoint=([a-z\-]+)', text):
                                        s['cc_entrypoint_in_system_prompt'][m.group(1)] += 1

                        # Anthropic event_logging
                        if '/event_logging' in url.path and isinstance(body, dict) and 'events' in body:
                            for ev in body['events']:
                                ed = ev.get('event_data', {}) if isinstance(ev, dict) else {}
                                if isinstance(ed, dict):
                                    key = f"{ed.get('entrypoint','?')}/{ed.get('is_interactive','?')}/{ed.get('client_type','?')}"
                                    s['event_log_classification'][key] += 1
                                    if 'agent_sdk_version' in ed:
                                        s['agent_sdk_version_sightings']['event_logging'] += 1
                                ev_name = ev.get('event_name') if isinstance(ev, dict) else None
                                if ev_name:
                                    s['event_log_event_names'][ev_name] += 1

                        # Datadog ddtags + nested
                        if 'datadoghq' in host and isinstance(body, list):
                            for ev in body:
                                tags = ev.get('ddtags', '') if isinstance(ev, dict) else ''
                                ep_t = ct_t = ii_t = '?'
                                ev_name = None
                                for p in tags.split(','):
                                    if p.startswith('entrypoint:'): ep_t = p[11:]
                                    elif p.startswith('client_type:'): ct_t = p[12:]
                                    elif p.startswith('event:'): ev_name = p[6:]
                                if isinstance(ev, dict) and 'is_interactive' in ev:
                                    ii_t = str(ev['is_interactive'])
                                s['datadog_classification'][f'{ep_t}/{ii_t}/{ct_t}'] += 1
                                if isinstance(ev, dict) and 'agent_sdk_version' in ev:
                                    s['agent_sdk_version_sightings']['datadog'] += 1
                                if ev_name:
                                    s['datadog_event_names'][ev_name] += 1

                        # Defensive: walk body, scan each string value, but skip
                        # paths that contain user-typed or model-output text
                        # (conversation history can legitimately reference any
                        # string, including 'sdk-cli' or 'agent_sdk_version' if
                        # the user is discussing those terms — that's content,
                        # not classification metadata).
                        scan_for_suspicious_strings(body, ep, s['suspicious_strings'])

            if resp:
                s['response_status_per_endpoint'][ep][resp.status_code] += 1
                rct = resp.headers.get('content-type', '')
                if rct:
                    s['response_content_types_per_endpoint'][ep][rct] += 1
    return s


# ────────────────────────────────────────────────────────────────────
# Helpers
# ────────────────────────────────────────────────────────────────────

SUSPICIOUS_PATTERNS = [
    (re.compile(r'sdk-(cli|py|ts|rs)'),       'sdk-* identifier (hyphen variant)'),
    # `_sdk_` substring with a negative lookahead for `mcp_sdk_*`. The
    # MCP SDK protocol library emits `mcp_sdk_connect` / `mcp_sdk_*`
    # event names from native CLI on MCP server connect — that's
    # Anthropic's own SDK protocol library, NOT forge's SDK shape, and
    # not a classification leak. Verified 2026-05-28 via #262's
    # debugger root-cause (zero grep hits for `feature_name` in forge
    # source). The negative lookbehind preserves the original
    # `tengu_sdk_*` / forge-side `*_sdk_*` leak detection.
    (re.compile(r'(?<!mcp)_sdk_'),             '_sdk_ in event_name or similar (e.g., tengu_sdk_* events leak forge SDK architecture)'),
    (re.compile(r'agent-sdk/[0-9]'),          'agent-sdk version label'),
    (re.compile(r'"entrypoint":"(sdk-|agent)'),  'classification-field leak'),
    (re.compile(r'"client_type":"(sdk-|agent)'), 'classification-field leak'),
    (re.compile(r'"is_interactive":false'),    'is_interactive=false (must be true for cli)'),
    (re.compile(r'"agent_sdk_version":'),      'agent_sdk_version field present'),
]

def walk_paths(obj, prefix=''):
    """Yield 'path:type' strings for every leaf and intermediate node."""
    if isinstance(obj, dict):
        if not obj:
            yield f'{prefix}:{{}}'
        else:
            for k, v in obj.items():
                yield from walk_paths(v, f'{prefix}.{k}' if prefix else k)
    elif isinstance(obj, list):
        if not obj:
            yield f'{prefix}:[]'
        else:
            # use [N] not [0] so different-length arrays at same path collapse
            for i, item in enumerate(obj):
                yield from walk_paths(item, f'{prefix}[N]' if i == 0 else f'{prefix}[N]')
    else:
        # leaf — record path + python type
        yield f'{prefix}:{type(obj).__name__}'


USER_CONTENT_PATH_FRAGMENTS = (
    # /v1/messages — these paths carry user-typed text or model output, which
    # can legitimately reference any string. Skip them in the suspicious scan
    # to avoid false positives when the user chats about the rewriter itself.
    'messages[N].content',     # any user/assistant message blocks
    'messages[N].content[N]',  # any block under messages content
    # tool_result content is also user-driven (tool outputs flow back as text)
    # but it's nested inside messages so already covered by 'messages[N].content'
    # NOTE: .system[N].text is NOT excluded — that's claude-constructed and
    # carries cc_entrypoint legitimately. The dedicated cc_entrypoint check
    # handles it separately, but the defensive scan should also catch any
    # other leak in the system prompt.
)

def is_user_content_path(path):
    return any(frag in path for frag in USER_CONTENT_PATH_FRAGMENTS)

def scan_for_suspicious_strings(body, ep, counter):
    """Walk body, find each string leaf, scan against SUSPICIOUS_PATTERNS.
    Skips user-content paths to avoid false positives from chat transcripts."""
    def walk(obj, path=''):
        if isinstance(obj, dict):
            for k, v in obj.items():
                walk(v, f'{path}.{k}' if path else k)
        elif isinstance(obj, list):
            for item in obj:
                walk(item, f'{path}[N]')
        elif isinstance(obj, str):
            if is_user_content_path(path):
                return  # skip user-typed / model-output text
            for pattern, label in SUSPICIOUS_PATTERNS:
                if pattern.search(obj):
                    counter[f"{label} @ {ep} (path={path})"] += 1
    walk(body)


def fingerprint(obj):
    """A short structural fingerprint of a JSON value (keys + types, no leaf values)."""
    if isinstance(obj, dict):
        keys = sorted(obj.keys())
        return f'{{{",".join(keys)}}}'
    if isinstance(obj, list):
        if not obj:
            return '[]'
        # fingerprint first element
        return f'[{fingerprint(obj[0])}...]'
    return type(obj).__name__


def load_accepted(path):
    if not path or not os.path.exists(path):
        return {'endpoints_in_alt_not_native': [],
                'endpoints_in_native_not_alt_acceptable': []}
    with open(path) as f:
        return json.load(f)


def matches_accepted(endpoint, accepted_list):
    parts = endpoint.split(' ', 1)
    if len(parts) != 2: return False
    host_path = parts[1]
    for entry in accepted_list:
        if entry.get('host', '') in host_path and entry.get('path_contains', '') in host_path:
            return True
    return False


# ────────────────────────────────────────────────────────────────────
# Diff dimensions — each returns list of (severity, message, detail)
# ────────────────────────────────────────────────────────────────────

def diff_bootstrap(n, a):
    out = []
    nb, ab = set(n['bootstrap_qs']), set(a['bootstrap_qs'])
    bad_n = nb - {'cli'}
    bad_a = ab - {'cli'}
    if bad_n:
        out.append(('FAIL', 'Bootstrap entrypoint qs (native)', f"native bootstrap_qs has non-cli values: {bad_n}"))
    if bad_a:
        out.append(('FAIL', 'Bootstrap entrypoint qs (forge)', f"forge bootstrap_qs has non-cli values: {bad_a}"))
    return out

def diff_msgs_ua(n, a):
    out = []
    msg_ep = 'POST api.anthropic.com/v1/messages'
    for label, side in [('native', n), ('forge', a)]:
        for ua in side['uas_per_endpoint'].get(msg_ep, {}):
            if 'sdk-' in ua or 'agent-sdk/' in ua:
                out.append(('FAIL', f'{label} /v1/messages UA leak', ua))
    return out

def diff_mcp_ua(n, a):
    out = []
    for ep in set(list(n['uas_per_endpoint']) + list(a['uas_per_endpoint'])):
        if '/mcp' not in ep: continue
        for label, side in [('native', n), ('forge', a)]:
            for ua in side['uas_per_endpoint'].get(ep, {}):
                if 'sdk-' in ua or 'agent-sdk/' in ua:
                    out.append(('FAIL', f'{label} MCP UA leak @ {ep}', ua))
    return out

def diff_telemetry_classification(n, a):
    """
    A telemetry classification value is a FAIL if:
      - It's not in the allowed set (cli/true/cli), AND
      - The OTHER side doesn't also produce the same value (mutual baseline is OK), AND
      - It's not the known parser-artifact 'claude/...' stray on datadog
    Events with `?/?/?` (no classification fields at all) appear naturally
    in both native and forge — they're event types that don't carry classification
    fields. Only FAIL if one side has them and the other doesn't.
    """
    out = []
    allowed_event = {'cli/True/cli', 'cli/true/cli'}
    allowed_dd = {'cli/true/cli', 'cli/True/cli'}

    for label, side, other in [('native', n, a), ('forge', a, n)]:
        for v in side['event_log_classification']:
            if v in allowed_event:
                continue
            # Mutual-baseline acceptance: if the other side also has this value,
            # it's not a forge-specific regression. Common for `?/?/?`.
            if v in other['event_log_classification']:
                continue  # both sides have it → baseline, not a leak
            out.append(('FAIL', f'{label} event_logging classification', f"saw {v} (must be cli/true/cli; other side does not exhibit)"))
        for v in side['datadog_classification']:
            if 'claude/' in v: continue  # known parser-artifact stray
            if v in allowed_dd:
                continue
            if v in other['datadog_classification']:
                continue  # mutual baseline
            out.append(('FAIL', f'{label} datadog classification', f"saw {v} (must be cli/true/cli; other side does not exhibit)"))
    return out

def diff_agent_sdk_version(n, a):
    out = []
    if n['agent_sdk_version_sightings']:
        out.append(('FAIL', 'native agent_sdk_version present', dict(n['agent_sdk_version_sightings'])))
    if a['agent_sdk_version_sightings']:
        out.append(('FAIL', 'forge agent_sdk_version present', dict(a['agent_sdk_version_sightings'])))
    return out

def diff_suspicious_strings(n, a):
    out = []
    for hit, count in a['suspicious_strings'].most_common():
        out.append(('FAIL', 'forge suspicious string', f"{count}x {hit}"))
    for hit, count in n['suspicious_strings'].most_common():
        # Anything native shows is a baseline observation — surface as INFO not FAIL
        out.append(('INFO', 'native suspicious string (baseline)', f"{count}x {hit}"))
    return out

def diff_system_prompt(n, a):
    out = []
    for label, side in [('native', n), ('forge', a)]:
        for v in side['cc_entrypoint_in_system_prompt']:
            if v != 'cli':
                out.append(('FAIL', f'{label} cc_entrypoint in /v1/messages system prompt',
                            f"saw '{v}' (must be 'cli')"))
    return out

def diff_endpoint_coverage(n, a, accepted):
    out = []
    n_eps = set(n['endpoints'])
    a_eps = set(a['endpoints'])
    only_a = a_eps - n_eps
    only_n = n_eps - a_eps
    for ep in sorted(only_a):
        if matches_accepted(ep, accepted.get('endpoints_in_alt_not_native', [])):
            out.append(('ACCEPTED', f'forge-only endpoint (accepted)', f"{ep} ({a['endpoints'][ep]}x)"))
        else:
            out.append(('FAIL', 'forge-only endpoint (unaccepted)', f"{ep} ({a['endpoints'][ep]}x)"))
    for ep in sorted(only_n):
        if matches_accepted(ep, accepted.get('endpoints_in_native_not_alt_acceptable', [])):
            out.append(('INFO', 'native-only endpoint (acceptable)', f"{ep} ({n['endpoints'][ep]}x)"))
        else:
            out.append(('WARN', 'native-only endpoint (unaccounted)',
                       f"{ep} ({n['endpoints'][ep]}x) — usually MCP-config noise, not a hard failure"))
    return out

def diff_b1_b3_regressions(n, a):
    out = []
    if a['count_tokens_hits'] > 0:
        out.append(('FAIL', 'B1 regression: forge hit /v1/messages/count_tokens',
                   f"{a['count_tokens_hits']}x (must be 0)"))
    if a['metrics_enabled_hits'] > 0:
        out.append(('FAIL', 'B3 regression: forge hit /metrics_enabled',
                   f"{a['metrics_enabled_hits']}x (must be 0)"))
    return out

def diff_headers_per_endpoint(n, a):
    """For each endpoint in BOTH captures, report headers that differ in name set or value set."""
    out = []
    common_eps = set(n['headers_per_endpoint']) & set(a['headers_per_endpoint'])
    for ep in sorted(common_eps):
        nh = n['headers_per_endpoint'][ep]
        ah = a['headers_per_endpoint'][ep]
        only_n_names = set(nh) - set(ah)
        only_a_names = set(ah) - set(nh)
        for name in only_n_names:
            out.append(('WARN', f'header only in native @ {ep}',
                       f"{name}: {list(nh[name])[:3]}"))
        for name in only_a_names:
            out.append(('WARN', f'header only in forge @ {ep}',
                       f"{name}: {list(ah[name])[:3]}"))
        # value diffs for same-name headers
        for name in set(nh) & set(ah):
            n_vals = nh[name]
            a_vals = ah[name]
            if n_vals != a_vals:
                only_n_vals = n_vals - a_vals
                only_a_vals = a_vals - n_vals
                if only_n_vals:
                    out.append(('WARN', f'header value only in native @ {ep}',
                               f"{name}: {list(only_n_vals)[:2]}"))
                if only_a_vals:
                    out.append(('WARN', f'header value only in forge @ {ep}',
                               f"{name}: {list(only_a_vals)[:2]}"))
    return out

def diff_query_params_per_endpoint(n, a):
    out = []
    common_eps = set(n['query_params_per_endpoint']) & set(a['query_params_per_endpoint'])
    for ep in sorted(common_eps):
        nq = n['query_params_per_endpoint'][ep]
        aq = a['query_params_per_endpoint'][ep]
        only_n = set(nq) - set(aq)
        only_a = set(aq) - set(nq)
        for k in only_n:
            out.append(('WARN', f'query param only in native @ {ep}',
                       f"{k}={list(nq[k])[:2]}"))
        for k in only_a:
            out.append(('WARN', f'query param only in forge @ {ep}',
                       f"{k}={list(aq[k])[:2]}"))
        for k in set(nq) & set(aq):
            if nq[k] != aq[k]:
                only_nv = nq[k] - aq[k]
                only_av = aq[k] - nq[k]
                if only_nv: out.append(('WARN', f'qparam value only in native @ {ep} ({k})', sorted(only_nv)[:3]))
                if only_av: out.append(('WARN', f'qparam value only in forge @ {ep} ({k})', sorted(only_av)[:3]))
    return out

def diff_body_paths_per_endpoint(n, a):
    """Report JSON paths in request bodies that appear in one side but not the other."""
    out = []
    common_eps = set(n['request_body_paths_per_endpoint']) & set(a['request_body_paths_per_endpoint'])
    for ep in sorted(common_eps):
        np = n['request_body_paths_per_endpoint'][ep]
        ap = a['request_body_paths_per_endpoint'][ep]
        only_n = np - ap
        only_a = ap - np
        if only_n:
            sample = sorted(only_n)[:5]
            out.append(('WARN', f'JSON path only in native body @ {ep}', sample))
        if only_a:
            sample = sorted(only_a)[:5]
            out.append(('WARN', f'JSON path only in forge body @ {ep}', sample))
    return out

def diff_anthropic_beta(n, a):
    out = []
    nb = set(n['anthropic_beta_values'])
    ab = set(a['anthropic_beta_values'])
    only_n = nb - ab
    only_a = ab - nb
    if only_n:
        out.append(('WARN', 'anthropic-beta value(s) only in native', list(only_n)[:3]))
    if only_a:
        out.append(('WARN', 'anthropic-beta value(s) only in forge', list(only_a)[:3]))
    return out

def diff_response_status(n, a):
    out = []
    common_eps = set(n['response_status_per_endpoint']) & set(a['response_status_per_endpoint'])
    for ep in sorted(common_eps):
        n_codes = set(n['response_status_per_endpoint'][ep])
        a_codes = set(a['response_status_per_endpoint'][ep])
        if n_codes != a_codes:
            out.append(('WARN', f'response status diff @ {ep}',
                       f"native={dict(n['response_status_per_endpoint'][ep])}, forge={dict(a['response_status_per_endpoint'][ep])}"))
    return out

def diff_event_names(n, a):
    out = []
    n_events = set(n['event_log_event_names']) | set(n['datadog_event_names'])
    a_events = set(a['event_log_event_names']) | set(a['datadog_event_names'])
    only_n = n_events - a_events
    only_a = a_events - n_events
    if only_n:
        out.append(('WARN', 'telemetry event_name only in native', sorted(only_n)[:8]))
    if only_a:
        out.append(('WARN', 'telemetry event_name only in forge', sorted(only_a)[:8]))
    return out


# ────────────────────────────────────────────────────────────────────
# Driver
# ────────────────────────────────────────────────────────────────────

def main():
    p = argparse.ArgumentParser()
    p.add_argument('--native', required=True)
    p.add_argument('--alt', required=True)
    p.add_argument('--accepted', default=None)
    p.add_argument('--audit-dir', default='audits/wire-equivalence')
    p.add_argument('--verbose', action='store_true')
    args = p.parse_args()

    n = survey(args.native)
    a = survey(args.alt)
    if n is None or a is None:
        print("ERROR: missing capture file(s)", file=sys.stderr)
        sys.exit(2)
    if n['flows'] == 0 or a['flows'] == 0:
        print(f"ERROR: empty capture. native={n['flows']} flows, forge={a['flows']} flows.", file=sys.stderr)
        sys.exit(2)

    accepted = load_accepted(args.accepted)

    print("=" * 78)
    print("WIRE-EQUIVALENCE — EXHAUSTIVE DIFF")
    print("=" * 78)
    print(f"  Native: {n['flows']} flows, {n['msgs']} /v1/messages, {len(n['sessions'])} session(s), {len(n['endpoints'])} distinct endpoints")
    print(f"  Forge : {a['flows']} flows, {a['msgs']} /v1/messages, {len(a['sessions'])} session(s), {len(a['endpoints'])} distinct endpoints")
    print()

    # Run every diff. Collect findings in priority order.
    all_findings = []
    for diff_fn, name in [
        (diff_bootstrap, 'Bootstrap entrypoint'),
        (diff_msgs_ua, '/v1/messages UA'),
        (diff_mcp_ua, 'MCP UA'),
        (diff_telemetry_classification, 'Telemetry classification'),
        (diff_agent_sdk_version, 'agent_sdk_version presence'),
        (diff_system_prompt, 'system prompt cc_entrypoint'),
        (diff_b1_b3_regressions, 'B1/B3 endpoint regression'),
        (diff_suspicious_strings, 'defensive string scan'),
    ]:
        findings = diff_fn(n, a) if diff_fn != diff_endpoint_coverage else diff_fn(n, a, accepted)
        all_findings.extend([(name, *f) for f in findings])

    # Endpoint coverage takes the accepted list
    for f in diff_endpoint_coverage(n, a, accepted):
        all_findings.append(('Endpoint coverage', *f))

    # The heavier "everything" diffs
    for diff_fn, name in [
        (diff_headers_per_endpoint, 'Header set / values per endpoint'),
        (diff_query_params_per_endpoint, 'Query params per endpoint'),
        (diff_body_paths_per_endpoint, 'JSON paths in request bodies'),
        (diff_anthropic_beta, 'anthropic-beta header values'),
        (diff_response_status, 'Response status codes per endpoint'),
        (diff_event_names, 'Telemetry event_name set'),
    ]:
        for f in diff_fn(n, a):
            all_findings.append((name, *f))

    # Print findings grouped by severity
    SEVERITY_ORDER = ['FAIL', 'WARN', 'INFO', 'ACCEPTED']
    by_severity = defaultdict(list)
    for finding in all_findings:
        section, sev, msg, detail = finding
        by_severity[sev].append((section, msg, detail))

    for sev in SEVERITY_ORDER:
        items = by_severity[sev]
        if not items: continue
        emoji = {'FAIL': '✗', 'WARN': '⚠', 'INFO': 'ℹ', 'ACCEPTED': '✓'}.get(sev, '·')
        print(f"\n{emoji} {sev}: {len(items)} finding(s)")
        for section, msg, detail in items[:30]:
            print(f"   [{section}] {msg}")
            if args.verbose or sev == 'FAIL':
                print(f"      → {detail}")
        if len(items) > 30:
            print(f"   ... and {len(items) - 30} more (use --verbose to see all)")

    # Final verdict
    n_fail = len(by_severity['FAIL'])
    n_warn = len(by_severity['WARN'])
    n_accepted = len(by_severity['ACCEPTED'])

    print()
    print("=" * 78)
    if n_fail == 0:
        print(f"VERDICT: ✓ PASS — zero classification failures.")
        if n_warn > 0:
            print(f"         {n_warn} non-classification difference(s) flagged as WARN (header/body/event-name diffs)")
            print(f"         These are normal cross-binary variation; review individually if you want to chase them.")
        if n_accepted > 0:
            print(f"         {n_accepted} accepted-divergence endpoint(s) confirmed in forge per accepted-divergences.json")
    else:
        print(f"VERDICT: ✗ FAIL — {n_fail} classification regression(s)")
        print(f"         Fix the FAIL items, then re-run this skill to verify.")
    print("=" * 78)

    # Write audit JSON for iterative comparison
    if args.audit_dir:
        os.makedirs(args.audit_dir, exist_ok=True)
        stamp = time.strftime('%Y-%m-%d-%H%M%S')
        audit = {
            'timestamp': stamp,
            'native_capture': args.native,
            'alt_capture': args.alt,
            'native_summary': {
                'flows': n['flows'], 'msgs': n['msgs'], 'endpoints': len(n['endpoints']),
            },
            'alt_summary': {
                'flows': a['flows'], 'msgs': a['msgs'], 'endpoints': len(a['endpoints']),
            },
            'findings_by_severity': {
                sev: [{'section': s, 'msg': m, 'detail': str(d)[:300]}
                      for (s, m, d) in by_severity[sev]]
                for sev in SEVERITY_ORDER
            },
            'fail_count': n_fail,
            'warn_count': n_warn,
            'accepted_count': n_accepted,
            'verdict': 'PASS' if n_fail == 0 else 'FAIL',
        }
        audit_path = os.path.join(args.audit_dir, f'audit-{stamp}.json')
        with open(audit_path, 'w') as f:
            json.dump(audit, f, indent=2)
        print(f"\nAudit JSON written: {audit_path}")
        # Compare with previous audit if one exists
        prior = sorted(os.listdir(args.audit_dir))
        prior_jsons = [p for p in prior if p.startswith('audit-') and p.endswith('.json') and p != f'audit-{stamp}.json']
        if prior_jsons:
            last = prior_jsons[-1]
            try:
                with open(os.path.join(args.audit_dir, last)) as f:
                    prev = json.load(f)
                delta_fail = n_fail - prev.get('fail_count', 0)
                delta_warn = n_warn - prev.get('warn_count', 0)
                sign = lambda x: f"+{x}" if x > 0 else str(x)
                print(f"Compared to previous run ({last}): FAIL {sign(delta_fail)}, WARN {sign(delta_warn)}")
            except Exception:
                pass

    sys.exit(0 if n_fail == 0 else 1)


if __name__ == '__main__':
    main()
