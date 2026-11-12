#!/usr/bin/env python3
"""Watch keyd's virtual F24: press records, release stops."""
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time
from evdev import InputDevice, ecodes, list_devices

VOICE = Path(__file__).with_name('voice.py')
CHUNKED = Path(__file__).with_name('chunked.py')
child = None
running = True

def shutdown(signum, frame):
    global running
    running = False

signal.signal(signal.SIGTERM, shutdown)
signal.signal(signal.SIGINT, shutdown)
while running:
    device = None
    for path in list_devices():
        try:
            candidate = InputDevice(path)
            if candidate.name == 'keyd virtual keyboard' and ecodes.KEY_F24 in candidate.capabilities().get(ecodes.EV_KEY, []):
                device = candidate
                break
            candidate.close()
        except OSError:
            pass
    if device is None:
        time.sleep(2)
        continue
    try:
        while running:
            if not select.select([device], [], [], 0.5)[0]:
                continue
            for event in device.read():
                if event.type != ecodes.EV_KEY or event.code != ecodes.KEY_F24:
                    continue
                if event.value == 1 and (child is None or child.poll() is not None):
                    command = [sys.executable, str(VOICE), 'record'] if os.environ.get('VELO_MODE') == 'batch' else [sys.executable, str(CHUNKED), '--insert']
                    child = subprocess.Popen(command)
                elif event.value == 0 and child is not None and child.poll() is None:
                    # Give record.py time to install its signal handler on quick taps.
                    time.sleep(0.15)
                    child.send_signal(signal.SIGTERM)
                    child = None
    except OSError:
        pass
    finally:
        device.close()
        if child is not None and child.poll() is None:
            child.send_signal(signal.SIGTERM)
        child = None
