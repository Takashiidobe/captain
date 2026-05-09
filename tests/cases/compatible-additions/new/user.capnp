@0xbf5147cbbecf40c1;

struct User {
  id @0 :UInt64;
  primaryEmail @1 :Text;
  status @2 :Status;
  displayName @3 :Text;
}

enum Status {
  active @0;
  disabled @1;
  pending @2;
}
