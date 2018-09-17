echo '{ "id": 5, "method":"blockchain.block.get_header", "params": [0] }' | nc 127.0.0.1 51401
echo '{ "id": 5, "method":"blockchain.block.get_header", "params": [1] }' | nc 127.0.0.1 51401
echo '{ "id": 5, "method":"blockchain.block.get_header", "params": [2] }' | nc 127.0.0.1 51401

