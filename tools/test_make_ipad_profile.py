import plistlib
import unittest
import uuid

from make_ipad_profile import make_profile


class ProfileTests(unittest.TestCase):
    def test_profile_round_trip_and_password_absence(self):
        profile = make_profile("breadcloud.breadway.dev", "breadway")
        encoded = plistlib.dumps(profile)
        self.assertEqual(plistlib.loads(encoded), profile)
        self.assertNotIn(b"Password", encoded)
        self.assertFalse(profile["PayloadRemovalDisallowed"])
        payloads = profile["PayloadContent"]
        self.assertEqual(len(payloads), 2)
        identifiers = [profile["PayloadUUID"]] + [p["PayloadUUID"] for p in payloads]
        self.assertEqual(len(set(identifiers)), 3)
        for identifier in identifiers:
            uuid.UUID(identifier)
        for payload, prefix in zip(payloads, ("CalDAV", "CardDAV")):
            self.assertTrue(payload[prefix + "UseSSL"])
            self.assertEqual(payload[prefix + "Port"], 443)
            self.assertEqual(payload[prefix + "PrincipalURL"],
                             "https://breadcloud.breadway.dev/remote.php/dav/principals/breadway/")
        self.assertEqual([p["PayloadType"] for p in payloads],
                         ["com.apple.caldav.account", "com.apple.carddav.account"])

    def test_invalid_host_and_account_rejected(self):
        for host in ("https://example.com", "example.com:443", "example.com/path", "user@example.com"):
            with self.subTest(host=host), self.assertRaises(ValueError):
                make_profile(host, "breadway")
        for user in ("", "../breadway", "breadway\\other", "breadway:secret", "breadway\n"):
            with self.subTest(user=user), self.assertRaises(ValueError):
                make_profile("example.com", user)


if __name__ == "__main__":
    unittest.main()
