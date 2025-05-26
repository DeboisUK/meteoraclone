This repo is a copy of the exact meteora repos that you will need.

You should understand the code fully, then use what you have learned to build a rust src/ directory.

The rust project you will build should;
  - receive live raw meteora dlmm pool data through a websocke
  - decode that raw data correctly (you will need to use the structs,discriminators etc from the meteora repos)
  - calculate optimal swap quotes accuratly (eventually this will be one part of a multidex arbitrage bot, so the swap quote calculations must be as precise as possible)
  - build correct transactions for those swap quotes optimally
  - serialize the correct, optimal swap transaction base64
