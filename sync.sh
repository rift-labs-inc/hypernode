# Helper script to compile contract artifacts and move them to artifacts/ dir 
git submodule update --init --remote
cd contracts/ && forge compile --via-ir && cd ..
rm -rf artifacts/*
mv contracts/out/RiftExchange.sol/RiftExchange.json artifacts/
mv contracts/out/RiftExchange.sol/IERC20.json artifacts/ 
mv contracts/out/MockUSDT.sol/MockUSDT.json artifacts/
