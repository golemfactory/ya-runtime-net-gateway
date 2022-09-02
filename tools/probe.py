#!/usr/bin/env python3

import asyncio
import ipaddress
import os
import typing
import urllib.parse

import click
import httpx

TOKEN_SIZE = 32

class BaseURL(str):
    def __truediv__(self, other):
        return type(self)(urllib.parse.urljoin(self, other))

class Addr(typing.NamedTuple):
    host: ipaddress._BaseAddress
    port: int

    @classmethod
    def fromstr(cls, value):
        split = urllib.parse.urlsplit('//' + value)
        if split.port is None:
            raise ValueError('no port specified')

        # ip_address() will throw on dnsname
        return cls(ipaddress.ip_address(split.hostname), split.port)

    def __str__(self):
        if isinstance(self.host, ipaddress.IPv4Address):
            return f'{self.host}:{self.port}'
        return f'[{self.host}]:{self.port}'

    def listen_on_any(self):
        return type(self)(
            ipaddress.ip_network(self.host).supernet(new_prefix=0).network_address,
            self.port)

async def probe(api, addr):
    token = os.urandom(TOKEN_SIZE)
    print(f'send token: {token}')
    async with httpx.AsyncClient() as http:
        resp = await http.post(api / '/probe-api/v1/tcp', json={
            'token': tuple(token),  # TODO maybe base64 this?
            'listen': str(addr.listen_on_any()),
        })
        resp.raise_for_status()
        data = resp.json()
        probe_id = data.pop('id')
        assert not data

        try:
            reader, writer = await asyncio.open_connection(
                str(addr.host), addr.port)
            remote_token = await reader.read(TOKEN_SIZE)
            print(f'read token: {remote_token}')
            writer.close()
            result = remote_token == token
            print(f'match: {result}')
            return result

        finally:
            resp = await http.delete(api / '/probe-api/v1/tcp/' / str(probe_id))
            resp.raise_for_status()


def validate_addr(ctx, value):
    return Addr.fromstr(value)

@click.command()
@click.option('--api', type=BaseURL, default='http://localhost:8000')
@click.argument('addr', callback=validate_addr, metavar='HOST:PORT')
def main(api, addr):
    asyncio.run(probe(api, addr))
if __name__ == '__main__':
    main()
