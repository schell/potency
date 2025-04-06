# potency
`potency` is a bare-bones durability and synchronization library meant to aid in writing idempotent processes without having to think too much.

For some background on durability, check out: 

https://flawless.dev/docs/

The idea is roughly that the results of certain "expensive" and fallible processes are cached with a key known by your code, based on namespaces and input parameters. 
Before running an expensive fallible process, the cache is queried by this known key and if the cache is hit, the result is read out of the cache instead of being computed.

`potency` offers some type-level machinery to abstract over multiple persistance layers as well as support for multi-colored functions (sync and async). Keep in mind the 
runtime itself is asynchronous, but sync functions can be used as the "process".

## goals

* replace bespoke persistance and idempontency processes with `potency`+ your raw operations
* local store support for
  - [x] in memory
  - [ ] flat-file JSON
  - [ ] sqlite
* remote store support for 
  - [ ] AWS Dynamo DB
  - [ ] Postgres
* multi-color support
  - [x] sync
  - [x] async
* [ ] easy key generation
* [ ] migrations
