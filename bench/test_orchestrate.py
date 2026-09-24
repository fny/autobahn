#!/usr/bin/env python3
"""Local checks for orchestrate.py's handling of values read back from
instances. No AWS and no network: `run` is replaced by a stand-in that
plays each ssh command's remote side in a local shell, under a scratch
HOME, so a value that escaped its quoting would really run.

    python3 bench/test_orchestrate.py
"""

import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import orchestrate  # noqa: E402

GENUINE = ("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH0dBnE1mgSv4p8dLl6A0VYbq1bYyC3Z"
           "5JQmU2m3+4q7 ubuntu@ip-10-0-0-1")


class Fleet:
    """Stands in for `orchestrate.run`: every command must be an argument
    list; an ssh command's remote string runs in bash with HOME set to a
    scratch directory, except that reading the public key returns
    whatever the test says the instance answered."""

    def __init__(self, home, public_key):
        self.home, self.public_key, self.commands = home, public_key, []

    def __call__(self, arguments, check=True, capture=True, input=None):
        assert isinstance(arguments, list), arguments
        self.commands.append(arguments)
        assert arguments[0] == "ssh", arguments
        remote = arguments[-1]
        if remote == "cat ~/.ssh/id_ed25519.pub":
            return subprocess.CompletedProcess(arguments, 0, self.public_key + "\n")
        if remote.startswith("ssh-keygen"):
            return subprocess.CompletedProcess(arguments, 0, "")
        done = subprocess.run(["bash", "-c", remote], env={**os.environ, "HOME": self.home},
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                              check=check, input=input)
        return done


class ValuesFromInstances(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.home = self.scratch.name
        os.makedirs(f"{self.home}/.ssh")
        self.saved = orchestrate.run

    def tearDown(self):
        orchestrate.run = self.saved
        self.scratch.cleanup()

    def test_a_host_key_carrying_a_command_is_rejected_and_nothing_runs(self):
        pwned = f"{self.home}/pwned"
        for hostile in (f"'; touch {pwned}; '",
                        f"{GENUINE}'; touch {pwned}; '",
                        f"{GENUINE}\ntouch {pwned}",
                        f"$(touch {pwned})",
                        f'"; touch {pwned}; "'):
            fleet = Fleet(self.home, hostile)
            orchestrate.run = fleet
            with self.assertRaises(ValueError, msg=hostile):
                orchestrate.authorize_source_key("k", "198.51.100.1", ["198.51.100.2"])
            self.assertFalse(os.path.exists(pwned), hostile)
            self.assertFalse(os.path.exists(f"{self.home}/.ssh/authorized_keys"), hostile)
            # Nothing was sent to the destination at all.
            self.assertFalse(any("ubuntu@198.51.100.2" in c for c in fleet.commands))

    def test_a_genuine_host_key_is_authorized_verbatim(self):
        orchestrate.run = Fleet(self.home, GENUINE)
        orchestrate.authorize_source_key("k", "198.51.100.1", ["198.51.100.2"])
        with open(f"{self.home}/.ssh/authorized_keys") as handle:
            self.assertEqual(handle.read(), GENUINE + "\n")

    def test_run_refuses_a_shell_string(self):
        with self.assertRaises(TypeError):
            orchestrate.run("true; touch /tmp/never")

    def test_checked_values(self):
        self.assertEqual(orchestrate.checked_public_key(f"  {GENUINE}\n"), GENUINE)
        for bad in ("", "ssh-dss AAAA", "ssh-ed25519", "ssh-ed25519 AAAA ok ok",
                    f"{GENUINE}\n{GENUINE}"):
            with self.assertRaises(ValueError, msg=bad):
                orchestrate.checked_public_key(bad)
        self.assertEqual(orchestrate.checked_address("10.0.0.7\n"), "10.0.0.7")
        for bad in ("", "10.0.0.7; true", "$(id)", "10.0.0.7 10.0.0.8"):
            with self.assertRaises(ValueError, msg=bad):
                orchestrate.checked_address(bad)
        self.assertEqual(orchestrate.checked_commit("a" * 40 + "\n"), "a" * 40)
        self.assertIsNone(orchestrate.checked_commit("'; id; '"))


if __name__ == "__main__":
    unittest.main()
