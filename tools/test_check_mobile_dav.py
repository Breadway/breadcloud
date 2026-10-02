import unittest

from check_mobile_dav import same_origin, successful_properties


class DiscoverySafetyTests(unittest.TestCase):
    def test_relative_href_remains_on_origin(self):
        self.assertEqual(same_origin("https://example.com/", "/dav/"), "https://example.com/dav/")

    def test_rejects_credential_exfiltration(self):
        for href in ("https://other.example/dav", "http://example.com/dav",
                     "https://example.com:444/dav", "//other.example/dav",
                     "https://user:secret@example.com/dav", "/dav/#fragment"):
            with self.subTest(href=href), self.assertRaises(ValueError):
                same_origin("https://example.com/", href)

    def test_failed_property_is_not_discovery_success(self):
        xml = '''<d:multistatus xmlns:d="DAV:"><d:response><d:href>/</d:href>
        <d:propstat><d:prop><d:current-user-principal><d:href>/wrong/</d:href>
        </d:current-user-principal></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
        <d:propstat><d:prop><d:displayname>Root</d:displayname></d:prop>
        <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>'''
        [(href, props)] = successful_properties(xml)
        self.assertEqual(href, "/")
        self.assertNotIn("{DAV:}current-user-principal", props)
        self.assertIn("{DAV:}displayname", props)

    def test_rejects_non_dav_xml(self):
        with self.assertRaises(ValueError):
            list(successful_properties("<html/>"))


if __name__ == "__main__":
    unittest.main()
