"""The real TUI in a private PTY for scripts/runtime-smoke.py and scripts/test-task.py.

`TuiPty` starts a program on a PTY, reads its output in a thread (a full PTY blocks the
program's draw, and then its keys) and keeps the screen the output paints.
"""
import codecs
import fcntl
import os
import re
import signal
import struct
import subprocess
import termios
import threading
import time
import unicodedata

BLANK = (False, False, None)  # bold, background, true-colour foreground


class Screen:
    """The cells a TUI has painted, from its output stream.

    ratatui repaints only the cells that changed and skips the rest with a cursor move, so a
    word it draws over blank cells is not one run of bytes in the stream: only the grid shows
    the text the user sees.
    """
    TOKEN = re.compile(r"\x1b\[([0-9;?<>=]*)[ -/]*([@-~])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[^\[\]]|[\r\n]|[^\x00-\x1f]+")

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.lock = threading.Lock()
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pending = ""
        self.row = self.col = 0
        self.style = BLANK
        self.grid = [[" "] * cols for _ in range(rows)]
        self.styles = [[BLANK] * cols for _ in range(rows)]

    def clear(self, top, bottom, left, right):
        for row in range(top, bottom):
            self.grid[row][left:right] = [" "] * (right - left)
            self.styles[row][left:right] = [BLANK] * (right - left)

    def feed(self, data):
        with self.lock:
            text = self.pending + self.decoder.decode(data)
            self.pending = ""
            # ^ An escape sequence cut by the read waits for the rest of it.
            cut = text.rfind("\x1b")
            if cut >= 0 and not self.TOKEN.match(text, cut):
                text, self.pending = text[:cut], text[cut:]
            for token in self.TOKEN.finditer(text):
                self.apply(token)

    def apply(self, token):
        params, final = token.group(1), token.group(2)
        if final:
            numbers = [int(part) if part.isdigit() else 0 for part in params.lstrip("?").split(";")]
            first = numbers[0] or 1
            if final == "m":
                self.sgr([int(part) if part.isdigit() else 0 for part in params.split(";")] if params else [0])
            elif final in "Hf":
                self.row = min(first - 1, self.rows - 1)
                self.col = min(((numbers[1] if len(numbers) > 1 else 0) or 1) - 1, self.cols - 1)
            elif final == "A": self.row = max(self.row - first, 0)
            elif final == "B": self.row = min(self.row + first, self.rows - 1)
            elif final == "C": self.col = min(self.col + first, self.cols - 1)
            elif final == "D": self.col = max(self.col - first, 0)
            elif final == "G": self.col = min(first - 1, self.cols - 1)
            elif final == "J" and numbers[0] in (2, 3):
                self.clear(0, self.rows, 0, self.cols)
            elif final == "K":
                start, end = {0: (self.col, self.cols), 1: (0, self.col + 1), 2: (0, self.cols)}[numbers[0]]
                self.clear(self.row, self.row + 1, start, end)
            return
        text = token.group(0)
        if text == "\r": self.col = 0
        elif text == "\n": self.row = min(self.row + 1, self.rows - 1)
        elif not text.startswith("\x1b"):
            for char in text:
                if self.col < self.cols:
                    self.grid[self.row][self.col] = char
                    self.styles[self.row][self.col] = self.style
                    self.col += 2 if unicodedata.east_asian_width(char) in "WF" else 1

    def sgr(self, codes):
        # ^ 1/22 bold, 4x/10x/48 a background, 49 none, 0 resets; 38 and 48 carry a colour.
        bold, background, fg = self.style
        index = 0
        while index < len(codes):
            code = codes[index]
            if code == 0:
                bold = background = False
                fg = None
            elif code == 39:
                fg = None
            elif code == 38 and codes[index + 1:index + 2] == [2] and index + 4 < len(codes):
                fg = tuple(codes[index + 2:index + 5])
            elif code == 1:
                bold = True
            elif code == 22:
                bold = False
            elif 40 <= code <= 47 or 100 <= code <= 107:
                background = True
            elif code == 49:
                background = False
            if code in (38, 48):
                background = background or code == 48
                index += 2 if index + 1 < len(codes) and codes[index + 1] == 5 else 4
            index += 1
        self.style = (bold, background, fg)

    def text(self):
        with self.lock:
            return "\n".join("".join(row).rstrip() for row in self.grid)

    def cells(self, colors=False):
        """Rows of `(char, bold, background)`; with `colors`, of `(char, (r, g, b) or None)`."""
        with self.lock:
            return [[(char, style[2]) if colors else (char, *style[:2]) for char, style in zip(row, styles)]
                    for row, styles in zip(self.grid, self.styles)]


class TuiPty:
    def __init__(self, argv, cwd, env, rows=40, cols=120):
        master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        # ^ crossterm reads the window size from /dev/tty when the process has one. The pty is the
        # TUI's controlling terminal, as in a real terminal, so a resize of it is the size it reads.
        self.proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave,
                                     close_fds=True, start_new_session=True,
                                     preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0))
        os.close(slave)
        self.master, self.cols = master, cols
        self.raw = bytearray()  # every byte written, for the mode line's order and the capture
        self.view = Screen(rows, cols)
        self.last_output = time.monotonic()
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        try:
            while chunk := os.read(self.master, 65536):
                self.raw.extend(chunk)
                self.view.feed(chunk)
                self.last_output = time.monotonic()
        except OSError:
            pass
        finally:
            os.close(self.master)

    def text(self):
        return self.view.text()

    def cells(self, colors=False):
        return self.view.cells(colors)

    def send(self, keys):
        # ^ The reader closes the master when the TUI ends; a later write would reach a reused fd.
        if not self.reader.is_alive():
            raise OSError("the TUI has ended")
        os.write(self.master, keys.encode() if isinstance(keys, str) else keys)

    def resize(self, rows, cols):
        """Set the PTY's window size and tell the TUI, as a window drag does."""
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.proc.send_signal(signal.SIGWINCH)

    def wait_text(self, found, message, seconds=20, idle=0):
        """Wait until `found` (a regex, or a function of the screen text) matches and the output
        has paused for `idle` seconds (a frame can arrive in several reads). Returns the text."""
        deadline = time.monotonic() + seconds
        while True:
            text = self.text()
            if time.monotonic() - self.last_output >= idle and (
                    found(text) if callable(found) else re.search(found, text)):
                return text
            if time.monotonic() >= deadline:
                shown = message() if callable(message) else message
                raise AssertionError(f"{shown} (TUI exit status {self.proc.poll()}):\n{text}")
            time.sleep(0.05)

    def close(self, quit_keys=None):
        """Quit with `quit_keys` when given, else end it with SIGTERM; kill it after 5 s."""
        if self.proc.poll() is None:
            if quit_keys:
                self.send(quit_keys)
            else:
                self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=3)
        self.reader.join(timeout=5)
