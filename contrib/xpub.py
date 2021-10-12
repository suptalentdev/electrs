#!/usr/bin/env python3
import argparse
import hashlib
import sys

from logbook import Logger, StreamHandler

from pycoin.symbols.btc import network

import client

log = Logger("xpub")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--host', default='localhost')
    parser.add_argument('xpub')
    args = parser.parse_args()

    conn = client.Client((args.host, 50001))
    total = 0
    xpub = network.parse.bip32(args.xpub)

    if xpub is None:
        log.error('Invalid BIP32 pub key %s' % args.xpub)
        sys.exit(1)

    for change in (0, 1):
        empty = 0
        for n in range(1000):
            address = xpub.subkey(change).subkey(n).address()
            script = network.parse.address(address).script()
            script_hash = hashlib.sha256(script).digest()[::-1].hex()
            result, = conn.call([client.request('blockchain.scripthash.get_history', script_hash)])
            ntx = len(result)
            result, = conn.call([client.request('blockchain.scripthash.get_balance', script_hash)])
            log.info('{}/{}: {} -> {} BTC confirmed, {} BTC unconfirmed, {} txs', change, n, address, result["confirmed"], result["unconfirmed"], ntx)

            confirmed = result['confirmed'] / 1e8
            total += confirmed
            if confirmed or ntx:
                empty = 0
            else:
                empty += 1
                if empty >= 20:
                    break
    log.info('total balance: {} BTC', total)


if __name__ == '__main__':
    with StreamHandler(sys.stderr, level='INFO').applicationbound():
        main()
