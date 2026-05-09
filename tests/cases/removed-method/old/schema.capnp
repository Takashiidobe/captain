@0xdcab8777d94650d5;

interface Users {
  get @0 (id :UInt64) -> (email :Text);
  list @1 () -> (count :UInt32);
}
