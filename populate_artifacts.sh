# Helper script to compile contract artifacts and move them to artifacts/ dir 
cd contracts/ && forge compile && cd ..
rm -rf artifacts/*
mv contracts/out/RiftExchange.sol/RiftExchange.json artifacts/
mv contracts/out/WETH.sol/WETH.json artifacts/
mv contracts/out/RiftExchange.sol/IERC20.json artifacts/ 
mv contracts/out/MockUSDT.sol/MockUSDT.json artifacts/
