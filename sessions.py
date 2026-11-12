"""Persistent local dictation sessions and resumable fragment processing."""
import contextlib
import fcntl
import json
import os
from pathlib import Path
import shutil
import time
import urllib.error
import uuid
from datetime import datetime, timezone
import voice

ROOT = Path(os.environ.get('XDG_STATE_HOME', Path.home() / '.local/state')) / 'velo' / 'sessions'


def write(path, data):
    temporary = path.with_suffix(path.suffix + '.tmp')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump(data, f, ensure_ascii=False)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temporary, path)


def create():
    ROOT.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(ROOT, 0o700)
    path = ROOT / uuid.uuid4().hex
    path.mkdir(mode=0o700)
    write(path / 'session.json', {'created': datetime.now(timezone.utc).isoformat(), 'status': 'recording', 'model': voice.MODEL})
    return path


def resolve(identifier):
    if identifier == 'last':
        paths = sorted(ROOT.glob('*/session.json'), key=lambda p: p.stat().st_mtime)
        if not paths:
            raise ValueError('Нет сохранённых диктовок')
        return paths[-1].parent
    if len(identifier) != 32 or any(c not in '0123456789abcdef' for c in identifier):
        raise ValueError('Неверный идентификатор сессии')
    path = ROOT / identifier
    if not (path / 'session.json').exists():
        raise ValueError('Сессия не найдена')
    return path


@contextlib.contextmanager
def lock(path):
    with (path / 'lock').open('a') as f:
        try:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError('Эта сессия ещё обрабатывается') from None
        yield


def status(path, value):
    metadata = json.loads((path / 'session.json').read_text())
    metadata['status'] = value
    write(path / 'session.json', metadata)


def retry(call, attempts_path):
    for attempt in range(3):
        try:
            result = call()
            return result
        except Exception as exc:
            # Store error class/status only, never a response body or API key.
            with attempts_path.open('a') as f:
                f.write(json.dumps({'time': time.time(), 'error': type(exc).__name__, 'http_status': getattr(exc, 'code', None), 'cost_unknown': True}) + '\n')
            transient = (isinstance(exc, urllib.error.HTTPError) and (exc.code == 429 or 500 <= exc.code < 600)) or (isinstance(exc, (urllib.error.URLError, TimeoutError, ConnectionError)) and not isinstance(exc, urllib.error.HTTPError))
            if not transient or attempt == 2:
                raise
            time.sleep(0.5 * (2 ** attempt))


def transcribe(path, pcm, number, api_key):
    from chunked import wav_bytes
    target = path / f'chunk-{number:06d}.json'
    if target.exists():
        saved = json.loads(target.read_text())
        return saved['text'], saved['usage']
    audio = path / f'chunk-{number:06d}.wav'
    audio.write_bytes(wav_bytes(pcm))
    text, usage = retry(lambda: voice.transcribe(audio, api_key, language='ru', return_usage=True), path / f'attempts-{number:06d}.jsonl')
    if not text:
        raise ValueError('Пустой ответ распознавания')
    write(target, {'text': text, 'usage': usage})
    audio.unlink(missing_ok=True)
    return text, usage


def save_text(path, text, name='raw.json'):
    write(path / name, {'text': text})


def finish(path):
    status(path, 'ready')
    for audio in path.glob('*.wav'):
        audio.unlink()
    (path / 'audio.pcm').unlink(missing_ok=True)


def cleanup(hours=24):
    if not ROOT.exists():
        return 0
    count = 0
    for metadata in ROOT.glob('*/session.json'):
        if time.time() - metadata.stat().st_mtime < hours * 3600:
            continue
        try:
            with lock(metadata.parent):
                shutil.rmtree(metadata.parent)
                count += 1
        except (OSError, RuntimeError):
            pass
    return count


def account(path, edit_status):
    import cost_stats
    chunks = [json.loads(p.read_text())['usage'] for p in sorted(path.glob('chunk-*.json'))]
    usage = path / 'edit-usage.json'
    editor = json.loads(usage.read_text()) if usage.exists() else None
    uncertain = any(path.glob('attempts-*.jsonl'))
    return cost_stats.save(chunks, editor, edit_status, session_id=path.name, uncertain=uncertain)
