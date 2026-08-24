import sys

def process(content):
    content = content.replace("IpAddr::V4(Ipv4Addr::new(\n            127, 0, 0, 1\n        ))", "IpAddr::V4(Ipv4Addr::LOCALHOST)")
    content = content.replace("IpAddr::V4(Ipv4Addr::new(\n            0, 0, 0, 0\n        ))", "IpAddr::V4(Ipv4Addr::UNSPECIFIED)")
    content = content.replace("IpAddr::V6(Ipv6Addr::new(\n            0, 0, 0, 0, 0, 0, 0, 1\n        ))", "IpAddr::V6(Ipv6Addr::LOCALHOST)")
    return content

with open("crates/openproxy-adapters/src/upstream/connector.rs", "r") as f:
    content = f.read()

content = process(content)

with open("crates/openproxy-adapters/src/upstream/connector.rs", "w") as f:
    f.write(content)
