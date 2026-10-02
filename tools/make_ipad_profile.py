#!/usr/bin/env python3
"""Generate a password-free, unsigned CalDAV/CardDAV profile for manual review."""

import argparse
import plistlib
import re
import urllib.parse
import uuid


def make_profile(host, user):
    if not re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?", host):
        raise ValueError("Host must be a DNS name, without scheme, port or path")
    if not user or any(character in user for character in "/\\:\r\n\x00"):
        raise ValueError("User must be a nonempty account name, without path separators or colon")
    principal = "https://" + host + "/remote.php/dav/principals/" + urllib.parse.quote(user, safe="") + "/"
    payloads = []
    for kind, prefix, label in (
        ("caldav", "CalDAV", "Breadcloud Calendar"),
        ("carddav", "CardDAV", "Breadcloud Contacts"),
    ):
        payloads.append({
            "PayloadType": f"com.apple.{kind}.account",
            "PayloadVersion": 1,
            "PayloadIdentifier": f"dev.breadway.breadcloud.{kind}",
            "PayloadUUID": str(uuid.uuid4()).upper(),
            "PayloadDisplayName": label,
            prefix + "AccountDescription": label,
            prefix + "HostName": host,
            prefix + "Username": user,
            prefix + "PrincipalURL": principal,
            prefix + "UseSSL": True,
            prefix + "Port": 443,
        })
    return {
        "PayloadType": "Configuration",
        "PayloadVersion": 1,
        "PayloadIdentifier": "dev.breadway.breadcloud.accounts",
        "PayloadUUID": str(uuid.uuid4()).upper(),
        "PayloadDisplayName": "Breadcloud Calendar and Contacts",
        "PayloadDescription": "Adds only two DAV accounts. Enter the account password on the device.",
        "PayloadRemovalDisallowed": False,
        "PayloadContent": payloads,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="breadcloud.breadway.dev")
    parser.add_argument("--user", default="breadway")
    parser.add_argument("--output", required=True, help="New .mobileconfig file; existing files are never replaced")
    args = parser.parse_args()
    try:
        profile = make_profile(args.host, args.user)
        with open(args.output, "xb") as output:
            plistlib.dump(profile, output, sort_keys=False)
    except (ValueError, OSError) as error:
        parser.exit(1, f"Profile generation failed: {error}\n")


if __name__ == "__main__":
    main()
