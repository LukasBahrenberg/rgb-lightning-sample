#!/usr/bin/env bash

source tests/common.sh

get_node_ids

# create RGB UTXOs
create_utxos 1
create_utxos 2

# issue asset
issue_asset