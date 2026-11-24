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
            "named-image": ('named-readonly-peer.c', '''
                char path[4096];
                struct stat image;
                return inspect_image(path, sizeof(path), &image);
            '''),
            "named-home": ('named-readonly-peer.c', '''
                struct stat image = {0};
                return run_helper("/unused", &image);
            '''),
            "responses-mode": ('capsule-responses-peer.c', '''
                char cap[78];
                memset(cap, '0', sizeof(cap) - 1);
                cap[77] = 0;
                const char *model = request_model(getenv("MODE"), cap);
                printf("%s:%c", model, cap[76]);
                return 0;
            '''),
            "named-helper": ('named-readonly-peer.c', '''
                struct stat image = {.st_dev = 12, .st_ino = 34};
                char *args[] = {"peer", "--helper", getenv("DEVICE"), getenv("INODE")};
                return check_helper(&image, args);
            '''),
            "responses-request": ('capsule-responses-peer.c', '''
                char request[2048];
                size_t bytes = format_request(request, sizeof(request), 4321,
                                              "local-cap", "offline-model");
                return fwrite(request, 1, bytes, stdout) == bytes ? 0 : 1;
            '''),
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
            prefix = "#define open fixture_open\n" if name.startswith("responses") else ""
            suffix = """
#undef open
int fixture_open(const char *path, int flags, ...) {
    (void)path;
    (void)flags;
    return -1;
}
""" if name.startswith("responses") else ""
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

    def test_named_image_and_home_refusals(self):
        result = subprocess.run([self.root / "named-image"], timeout=5)
        self.assertEqual(result.returncode, 61)  # Scratch image is outside the canonical route.
        for home in [None, "/tmp/unqualified"]:
            with self.subTest(home=home):
                environment = dict(os.environ)
                environment.pop("CODEX_HOME", None)
                if home is not None:
                    environment["CODEX_HOME"] = home
                result = subprocess.run([self.root / "named-home"], env=environment, timeout=5)
                self.assertEqual(result.returncode, 67)

    def test_responses_modes_preserve_model_and_key_selection(self):
        for mode, output in [
            ("positive", b"offline-model:0"),
            ("wrong-key", b"offline-model:1"),
            ("wrong-model", b"other-model:0"),
            ("cancel", b"offline-model:0"),
            ("expiry", b"offline-model:0"),
            ("unknown", b""),
        ]:
            with self.subTest(mode=mode):
                result = subprocess.run(
                    [self.root / "responses-mode"], env={**os.environ, "MODE": mode},
                    capture_output=True, timeout=5,
                )
                self.assertEqual(result.returncode, 102 if mode == "unknown" else 0)
                self.assertEqual(result.stdout, output)

    def test_named_helper_requires_same_device_and_inode(self):
        for device, inode, expected in [("12", "34", 0), ("13", "34", 63), ("12", "35", 63)]:
            with self.subTest(device=device, inode=inode):
                result = subprocess.run(
                    [self.root / "named-helper"],
                    env={**os.environ, "DEVICE": device, "INODE": inode},
                    capture_output=True, timeout=5,
                )
                self.assertEqual(result.returncode, expected)
                self.assertEqual(result.stdout, b"NAMED-HELPER:same-inode-canonical\n" if expected == 0 else b"")

    def test_responses_request_preserves_exact_wire_bytes(self):
        body = (b'{"model":"offline-model","stream":true,"store":false,'
                b'"input":[{"type":"message","role":"user","content":"finite capsule input"}],'
                b'"reasoning":{"effort":"medium"},"tools":[]}')
        expected = (b"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:4321\r\n"
                    b"Authorization: Bearer local-cap\r\nContent-Type: application/json\r\n"
                    + f"Content-Length: {len(body)}\r\n".encode()
                    + b"Connection: close\r\n\r\n" + body)
        result = subprocess.run([self.root / "responses-request"], capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, expected)

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
