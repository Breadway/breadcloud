#!/usr/bin/env python3
"""Read-only DAV discovery, or opt-in fixture checks on an isolated test server."""

import argparse
import base64
import getpass
import json
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
import xml.etree.ElementTree as ET

D = "DAV:"
C = "urn:ietf:params:xml:ns:caldav"
A = "urn:ietf:params:xml:ns:carddav"
NS = {"d": D, "c": C, "a": A}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def same_origin(base, href):
    url = urllib.parse.urljoin(base, href)
    a, b = urllib.parse.urlsplit(base), urllib.parse.urlsplit(url)
    if (a.scheme, a.hostname, a.port) != (b.scheme, b.hostname, b.port):
        raise ValueError("DAV discovery returned a different origin")
    if b.username or b.password or b.fragment:
        raise ValueError("DAV URL contains credentials or a fragment")
    return url


def successful_properties(body):
    root = ET.fromstring(body)
    if root.tag != "{DAV:}multistatus":
        raise ValueError("Expected DAV multistatus XML")
    for response in root.findall("d:response", NS):
        props = {}
        for stat in response.findall("d:propstat", NS):
            status = (stat.findtext("d:status", default="", namespaces=NS)).split()
            if len(status) > 1 and status[1] == "200":
                prop = stat.find("d:prop", NS)
                if prop is not None:
                    props.update({node.tag: node for node in prop})
        yield response.findtext("d:href", namespaces=NS), props


class Client:
    def __init__(self, base, user, password):
        self.base = base.rstrip("/") + "/"
        self.auth = "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()
        self.opener = urllib.request.build_opener(NoRedirect)
        self.trace = []

    def request(self, method, href, body=None, headers=None, expected=(207,), auth=True):
        url = same_origin(self.base, href)
        request_headers = {"User-Agent": "breadcloud-mobile-check/1"}
        if auth:
            request_headers["Authorization"] = self.auth
        if body is not None:
            body = body.encode() if isinstance(body, str) else body
            request_headers["Content-Type"] = "application/xml; charset=utf-8"
        request_headers.update(headers or {})
        req = urllib.request.Request(url, data=body, headers=request_headers, method=method)
        try:
            result = self.opener.open(req, timeout=20)
        except urllib.error.HTTPError as error:
            result = error
        with result:
            payload = result.read(2 * 1024 * 1024 + 1)
            if len(payload) > 2 * 1024 * 1024:
                raise ValueError("DAV check response exceeded 2 MiB")
            self.trace.append({"method": method, "path": urllib.parse.urlsplit(url).path,
                               "status": result.code})
            if result.code not in expected:
                raise ValueError(f"{method} returned HTTP {result.code}, expected {expected}")
            return payload, result.headers

    def propfind(self, href, properties, depth="0"):
        body = '<d:propfind xmlns:d="DAV:" xmlns:c="' + C + '" xmlns:a="' + A + '"><d:prop>'
        body += "".join(f"<{name}/>" for name in properties) + "</d:prop></d:propfind>"
        payload, _ = self.request("PROPFIND", href, body, {"Depth": depth})
        return list(successful_properties(payload))

    def property_href(self, href, property_name, tag):
        for _, props in self.propfind(href, [property_name]):
            prop = props.get(tag)
            if prop is not None:
                value = prop.findtext("d:href", namespaces=NS)
                if value:
                    return same_origin(self.base, value)
        raise ValueError(f"Missing successful {property_name} property")

    def discover(self, user):
        for name in ("caldav", "carddav"):
            _, headers = self.request("GET", "/.well-known/" + name,
                                      expected=(301, 302, 307, 308), auth=False)
            location = headers.get("Location")
            if not location:
                raise ValueError("Well-known redirect is missing Location")
            same_origin(self.base, location)
        self.request("PROPFIND", "/remote.php/dav/", headers={"Depth": "0"},
                     expected=(401,), auth=False)
        principal = self.property_href("/", "d:current-user-principal", "{DAV:}current-user-principal")
        calendar_home = self.property_href(principal, "c:calendar-home-set", "{" + C + "}calendar-home-set")
        addressbook_home = self.property_href(principal, "a:addressbook-home-set", "{" + A + "}addressbook-home-set")
        collections = {}
        for kind, home, resource_tag, compliance in (
            ("calendars", calendar_home, "{" + C + "}calendar", "calendar-access"),
            ("addressbooks", addressbook_home, "{" + A + "}addressbook", "addressbook"),
        ):
            collections[kind] = []
            for href, props in self.propfind(home, ["d:resourcetype", "d:displayname"], "1"):
                resource = props.get("{DAV:}resourcetype")
                if resource is not None and resource.find(resource_tag) is not None:
                    url = same_origin(self.base, href)
                    _, headers = self.request("OPTIONS", url, expected=(200, 204))
                    if compliance not in {value.strip() for value in headers.get("DAV", "").split(",")}:
                        raise ValueError(f"Missing {compliance} DAV capability")
                    collections[kind].append(url)
            if not collections[kind]:
                raise ValueError(f"No {kind} discovered")
        file_root = "/remote.php/dav/files/" + urllib.parse.quote(user, safe="") + "/"
        self.propfind(file_root, ["d:resourcetype", "d:getetag"])
        collections["files"] = [same_origin(self.base, file_root)]
        return collections

    def fixture_check(self, collections):
        """Only call with a disposable test account. DELETE can leave version/trash data."""
        identifier = "mobile-check-" + uuid.uuid4().hex
        objects = [
            (collections["calendars"][0] + identifier + ".ics", "text/calendar",
             "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//breadcloud//mobile check//EN\r\nBEGIN:VEVENT\r\n"
             f"UID:{identifier}\r\nDTSTART:20260930T090000Z\r\nDTEND:20260930T100000Z\r\n"
             "SUMMARY:Mobile protocol check\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"),
            (collections["addressbooks"][0] + identifier + ".vcf", "text/vcard",
             f"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{identifier}\r\nFN:Mobile protocol check\r\n"
             "N:Check;Mobile;;;\r\nEND:VCARD\r\n"),
            (collections["files"][0] + identifier + ".txt", "text/plain", "Mobile protocol check\n"),
        ]
        created = []
        try:
            for href, content_type, payload in objects:
                self.request("PUT", href, payload,
                             {"Content-Type": content_type, "If-None-Match": "*"}, (201,))
                created.append(href)
                content, headers = self.request("GET", href, expected=(200,))
                if content != payload.encode():
                    raise ValueError("Created fixture did not round trip")
                etag = headers.get("ETag")
                if not etag:
                    raise ValueError("Fixture GET is missing ETag")
                updated = payload.replace("Mobile protocol check", "Updated protocol check")
                self.request("PUT", href, updated, {"Content-Type": content_type, "If-Match": etag}, (204,))
                content, updated_headers = self.request("GET", href, expected=(200,))
                if content != updated.encode() or updated_headers.get("ETag") == etag:
                    raise ValueError("Updated fixture content or ETag did not change")
        finally:
            cleanup_errors = []
            for href in reversed(created):
                try:
                    self.request("DELETE", href, expected=(204,))
                    self.request("GET", href, expected=(404,))
                except (ValueError, OSError) as error:
                    cleanup_errors.append(f"{href}: {error}")
            if cleanup_errors:
                raise ValueError("Fixture cleanup failed: " + "; ".join(cleanup_errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password-stdin", action="store_true", help="Read one password line, never print it")
    parser.add_argument("--isolated-write-test", action="store_true", help="Create fixtures; disposable server/account only")
    args = parser.parse_args()
    parsed = urllib.parse.urlsplit(args.base_url)
    if parsed.scheme != "https" and not (
        parsed.scheme == "http" and parsed.hostname in ("127.0.0.1", "::1", "localhost")
    ):
        parser.error("Use HTTPS, or HTTP on loopback for an isolated server")
    if parsed.path not in ("", "/") or parsed.query or parsed.fragment or parsed.username:
        parser.error("Base URL must contain only scheme, host and optional port")
    password = sys.stdin.readline().rstrip("\r\n") if args.password_stdin else getpass.getpass()
    if not password:
        parser.error("Password must not be empty")
    client = Client(args.base_url, args.user, password)
    try:
        collections = client.discover(args.user)
        if args.isolated_write_test:
            client.fixture_check(collections)
        print(json.dumps({"checks": client.trace, "collections": collections,
                          "write_fixtures": args.isolated_write_test,
                          "physical_device_acceptance": "not tested"}, indent=2))
    except (ValueError, OSError, ET.ParseError) as error:
        print(f"Check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
