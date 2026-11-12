"""OpenRouter requests with an optional PlusVibeAPI.ru fallback."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

BASES = {'openrouter': 'https://openrouter.ai/api/v1', 'plusvibe': 'https://plusvibeapi.ru/v1'}
STATE = Path(os.environ.get('XDG_RUNTIME_DIR', '/tmp')) / f'voice-dictation-{os.getuid()}'
COOLDOWN = 120


def key(provider):
    variable = 'OPENROUTER_API_KEY' if provider == 'openrouter' else 'PLUSVIBE_API_KEY'
    value = os.environ.get(variable)
    if value:
        return value
    try:
        return subprocess.check_output(['secret-tool', 'lookup', 'application', 'voice-dictation', 'service', provider], stderr=subprocess.DEVNULL, text=True, timeout=5).strip()
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return ''


def primary_down():
    try:
        return json.loads((STATE / 'provider-health.json').read_text()).get('retry_after', 0) > time.time()
    except (OSError, ValueError, TypeError):
        return False


def mark_down():
    STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix='health-', dir=STATE)
    with os.fdopen(fd, 'w') as f:
        json.dump({'retry_after': time.time() + COOLDOWN}, f)
    os.replace(tmp, STATE / 'provider-health.json')


def unavailable(exc):
    if isinstance(exc, urllib.error.HTTPError):
        return exc.code in (403, 408, 429, 451) or 500 <= exc.code < 600
    return isinstance(exc, (urllib.error.URLError, TimeoutError, ConnectionError))


def request(endpoint, build, primary_key):
    """build(provider) -> (bytes, content_type, model). Never cross-use keys."""
    mode = os.environ.get('VELO_PROVIDER', 'auto')
    if mode not in ('auto', 'openrouter', 'plusvibe'):
        raise ValueError('VELO_PROVIDER должен быть auto, openrouter или plusvibe')
    if primary_key == 'plusvibe-only':
        primary_key = ''
    fallback_key = key('plusvibe') if mode != 'openrouter' else ''
    if mode == 'plusvibe':
        routes = [('plusvibe', fallback_key)]
    elif mode == 'openrouter':
        routes = [('openrouter', primary_key)]
    elif primary_down() and fallback_key:
        routes = [('plusvibe', fallback_key)]
    else:
        routes = [('openrouter', primary_key)] + ([('plusvibe', fallback_key)] if fallback_key else [])
    last_error = None
    uncertain = False
    for provider, credential in routes:
        if not credential:
            continue
        data, content_type, model = build(provider)
        req = urllib.request.Request(BASES[provider] + endpoint, data=data,
            headers={'Authorization': 'Bearer ' + credential, 'Content-Type': content_type})
        try:
            with urllib.request.urlopen(req, timeout=15 if provider == 'openrouter' and len(routes) > 1 else 45) as response:
                result = json.load(response)
        except (urllib.error.URLError, TimeoutError, ConnectionError) as exc:
            last_error = exc
            if mode != 'auto' or provider != 'openrouter' or not unavailable(exc) or not fallback_key:
                raise
            mark_down()
            uncertain = True
            continue
        usage = result.get('usage')
        usage = dict(usage) if isinstance(usage, dict) else {}
        # OpenRouter documents cost in USD. PlusVibe.ru bills in RUB;
        # retain its raw cost separately until the currency is confirmed.
        if provider == 'plusvibe' and 'cost' in usage:
            usage['provider_reported_cost'] = usage.pop('cost')
        usage.update({'provider': provider, 'model': model})
        if uncertain:
            usage['prior_attempt_cost_unknown'] = True
        result['usage'] = usage
        return result
    if last_error:
        raise last_error
    raise ValueError('Нет ключа для выбранного API-провайдера')
