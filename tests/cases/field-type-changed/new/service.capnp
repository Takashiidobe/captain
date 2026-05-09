@0x9b4100e8bb6de217;

interface UserService {
  get @0 (id :UInt64) -> (email :Text);
  list @1 () -> (count :UInt32);
}
