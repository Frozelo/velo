"""Runtime state shared by dictation and its optional status window."""
import os
from pathlib import Path
import subprocess
import sys
import sessions
import voice

def publish(phase, session=None):
    voice.STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    sessions.write(voice.STATE / 'ui.json', {'phase': phase, 'pid': os.getpid(), 'session': session})


def start():
    subprocess.Popen([sys.executable, str(Path(__file__).with_name('status_ui.py'))], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)

