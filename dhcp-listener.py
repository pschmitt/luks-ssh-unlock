#!/usr/bin/env python3

"""Watch DHCP ACK packets and start a validated one-shot unlock attempt."""

import argparse
import ipaddress
import logging
import os
import select
import socket
import struct
import subprocess
import threading
import time


def parse_dhcp_packet(packet):
    if len(packet) < 20:
        return None

    ip_header_length = (packet[0] & 0x0F) * 4
    if packet[9] != socket.IPPROTO_UDP or len(packet) < ip_header_length + 8 + 240:
        return None

    source_port, destination_port = struct.unpack_from("!HH", packet, ip_header_length)
    if (source_port, destination_port) not in ((67, 68), (68, 67)):
        return None

    bootp = packet[ip_header_length + 8 :]
    if bootp[236:240] != b"\x63\x82\x53\x63":
        return None

    operation = bootp[0]
    transaction_id = bootp[4:8]
    client_address = bootp[28:34]
    assigned_ip = str(ipaddress.ip_address(bootp[16:20]))
    options = {}
    offset = 240
    while offset < len(bootp):
        option = bootp[offset]
        offset += 1
        if option == 255:
            break
        if option == 0:
            continue
        if offset >= len(bootp):
            break
        length = bootp[offset]
        offset += 1
        value = bootp[offset : offset + length]
        offset += length
        options[option] = value

    if 53 not in options:
        return None

    client_hostname = options.get(12, b"").decode("ascii", errors="ignore").rstrip(".").lower()
    requested_ip = (
        str(ipaddress.ip_address(options[50]))
        if len(options.get(50, b"")) == 4
        else None
    )
    return (
        operation,
        options[53][0],
        transaction_id,
        client_address,
        client_hostname,
        assigned_ip,
        requested_ip,
    )


def wait_for_ssh(address, port, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((address, port), timeout=2):
                return True
        except OSError:
            time.sleep(2)
    return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--interface", required=True)
    parser.add_argument("--client-hostname", required=True)
    parser.add_argument("--target-hostname", required=True)
    parser.add_argument("--ssh-port", type=int, default=22)
    parser.add_argument("--ssh-wait-timeout", type=int, default=180)
    parser.add_argument("--environment-file", required=True)
    parser.add_argument("--unlocker", required=True)
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
        datefmt="%Y-%m-%dT%H:%M:%S%z",
    )
    logging.info(
        "Listening for DHCP ACKs for %s on %s",
        args.client_hostname,
        args.interface,
    )
    try:
        target_addresses = {
            result[4][0]
            for result in socket.getaddrinfo(
                args.target_hostname,
                None,
                socket.AF_INET,
                socket.SOCK_STREAM,
            )
        }
    except socket.gaierror:
        target_addresses = set()
        logging.warning("Could not resolve target address for %s", args.target_hostname)

    active_addresses = set()
    recent_addresses = {}
    pending_clients = {}
    lock = threading.Lock()

    def handle_ack(address):
        try:
            logging.info("DHCP assigned %s to %s; waiting for SSH", address, args.client_hostname)
            if not wait_for_ssh(address, args.ssh_port, args.ssh_wait_timeout):
                logging.warning("SSH did not become ready at %s within %ss", address, args.ssh_wait_timeout)
                return

            logging.info("SSH is ready at %s; starting validated unlock for %s", address, args.target_hostname)
            unit = f"luks-ssh-unlock-dhcp-{args.client_hostname}-{int(time.time())}"
            command = [
                "systemd-run",
                "--system",
                "--wait",
                "--pipe",
                "--collect",
                f"--unit={unit}",
                f"--property=EnvironmentFile={args.environment_file}",
                f"--setenv=SSH_CONNECT_ADDRESS={address}",
                f"--setenv=SSH_HOSTKEY_ALIAS={args.target_hostname}",
            ]
            credential_directory = os.environ.get("CREDENTIALS_DIRECTORY")
            if credential_directory:
                credential_path = os.path.join(credential_directory, "luks-passphrase")
                if os.path.isfile(credential_path):
                    command.append(f"--property=LoadCredential=luks-passphrase:{credential_path}")
            command.extend(["--", args.unlocker, "run", "--once"])
            result = subprocess.run(command, check=False)
            if result.returncode == 0:
                logging.info("Validated unlock attempt completed for %s", args.target_hostname)
            else:
                logging.error("Unlock attempt for %s failed with status %s", args.target_hostname, result.returncode)
        finally:
            with lock:
                active_addresses.discard(address)

    interfaces = (
        [name for _, name in socket.if_nameindex()]
        if args.interface == "any"
        else [args.interface]
    )
    sockets = []
    for interface in interfaces:
        try:
            sock = socket.socket(socket.AF_PACKET, socket.SOCK_DGRAM, 0x0800)
            sock.bind((interface, 0x0800))
            sockets.append(sock)
        except OSError as error:
            logging.warning("Could not listen on %s: %s", interface, error)
    if not sockets:
        raise RuntimeError("Could not bind a DHCP listener socket to any interface")

    while True:
        readable, _, _ = select.select(sockets, [], [])
        for sock in readable:
            packet, _ = sock.recvfrom(4096)
            dhcp_packet = parse_dhcp_packet(packet)
            if dhcp_packet is None:
                continue

            (
                operation,
                message_type,
                transaction_id,
                client_address,
                client_hostname,
                address,
                requested_address,
            ) = dhcp_packet
            if operation == 1 and message_type in (1, 3):
                if (
                    client_hostname != args.client_hostname.lower()
                    and requested_address not in target_addresses
                ):
                    continue
                pending_clients[client_address] = (transaction_id, time.monotonic())
                logging.info("Detected DHCP request from %s; waiting for its lease", args.client_hostname)
                continue

            if operation != 2 or message_type != 5:
                continue

            pending = pending_clients.get(client_address)
            if pending is not None and time.monotonic() - pending[1] > 180:
                pending_clients.pop(client_address, None)
                pending = None
            if client_hostname != args.client_hostname.lower() and pending is None:
                continue
            if pending is not None:
                pending_clients.pop(client_address, None)

            now = time.monotonic()
            with lock:
                if address in active_addresses or now - recent_addresses.get(address, 0) < 60:
                    continue
                active_addresses.add(address)
                recent_addresses[address] = now

            threading.Thread(target=handle_ack, args=(address,), daemon=True).start()


if __name__ == "__main__":
    main()
