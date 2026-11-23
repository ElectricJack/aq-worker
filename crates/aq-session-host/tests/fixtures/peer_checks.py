"""Focused fixture checks without namespaces, mounted auth, or a provider."""
import fcntl
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class PeerChecks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.scratch = tempfile.TemporaryDirectory(prefix="aq-peer-checks-", dir="/tmp")
        cls.root = Path(cls.scratch.name)
        fixtures = Path(__file__).resolve().parent
        wrappers = {
            "named": ('named-readonly-peer.c', 'return check_stdin();'),
            "static": ('capsule-static-peer.c', 'check_stdin(); return 0;'),
            "responses": ('capsule-responses-peer.c', '''
                char body[16384] = {0};
                size_t bytes = fread(body, 1, sizeof(body) - 1, stdin);
                check_response(body, bytes);
                return 0;
            '''),
        }
        for name, (fixture, body) in wrappers.items():
            source = cls.root / (name + ".c")
            # Parser refusals normally write an absolute guest-error marker.
            # Disable opens in this wrapper so checks cannot write outside scratch.
            prefix = "#define open fixture_open\n" if name == "responses" else ""
            suffix = """
#undef open
int fixture_open(const char *path, int flags, ...) {
    (void)path;
    (void)flags;
    return -1;
}
""" if name == "responses" else ""
            source.write_text(
                prefix +
                '#define main fixture_main\n'
                f'#include "{fixture}"\n'
                '#undef main\n'
                f'int main(void) {{ {body} }}\n' + suffix
            )
            subprocess.run(
                ["cc", "-static", "-O2", "-Wall", "-Wextra", "-Werror",
                 "-I", str(fixtures), str(source), "-o", str(cls.root / name)],
                check=True, timeout=30,
            )

    @classmethod
    def tearDownClass(cls):
        cls.scratch.cleanup()

    def test_named_stdin_bytes_bounds_and_seals(self):
        payload = b"named readonly sealed stdin\n"
        for body, sealed, expected in [
            (payload, True, 0),
            (payload, False, 66),
            (payload[:-1], True, 66),
            (b"changed\n", True, 66),
            (b"x" * 128, True, 65),
        ]:
            with self.subTest(body=body, sealed=sealed):
                fd = os.memfd_create("fixture-stdin", os.MFD_ALLOW_SEALING)
                try:
                    os.write(fd, body)
                    os.lseek(fd, 0, os.SEEK_SET)
                    if sealed:
                        fcntl.fcntl(fd, fcntl.F_ADD_SEALS,
                                    fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW |
                                    fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL)
                    result = subprocess.run([self.root / "named"], stdin=fd, timeout=5)
                    self.assertEqual(result.returncode, expected)
                finally:
                    os.close(fd)

    def test_static_stdin_is_exact(self):
        payload = b"capsule exact stdin bytes\n"
        for body, expected in [(payload, 0), (payload[:-1], 101), (payload + b"x", 101)]:
            with self.subTest(body=body):
                result = subprocess.run([self.root / "static"], input=body, timeout=5)
                self.assertEqual(result.returncode, expected)

    def test_responses_accepts_chunked_sse_and_refuses_malformed_frames(self):
        headers = (b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n"
                   b"Transfer-Encoding: chunked\r\n\r\n")
        event = (b'event: response.completed\ndata: {"type":"response.completed",'
                 b'"marker":"finite-capsule-sse"}\n\n')
        chunk = f"{len(event):x}".encode() + b"\r\n" + event + b"\r\n"
        split = b"A\r\n" + event[:10] + b"\r\n"
        split += f"{len(event) - 10:X}".encode() + b"\r\n" + event[10:] + b"\r\n"
        for frame, accepted in [
            (headers + chunk + b"0\r\n\r\n", True),
            (headers + split + b"0\r\n\r\n", True),
            (headers + chunk, False),
            (headers + chunk + b"0\r\n\r\nextra", False),
            (headers + b"g\r\n", False),
            (headers + b"\r\n", False),
            (headers + b"000000001\r\nX\r\n0\r\n\r\n", False),
            (headers + b"1\r\nXx\n0\r\n\r\n", False),
            (headers + b"800\r\n" + b"x" * 2048 + b"\r\n" +
             b"800\r\n" + b"x" * 2048 + b"\r\n0\r\n\r\n", False),
            (headers + b"1000\r\n", False),
            (headers + b"0\r\n\r\n", False),
            (headers.replace(b"200 OK", b"403 NO") + chunk + b"0\r\n\r\n", False),
        ]:
            with self.subTest(frame=frame):
                # Marker opens are disabled above: a parser refusal exits 102.
                result = subprocess.run([self.root / "responses"], input=frame, timeout=5)
                if accepted:
                    self.assertEqual(result.returncode, 0)
                else:
                    self.assertEqual(result.returncode, 102)


if __name__ == "__main__":
    unittest.main()
