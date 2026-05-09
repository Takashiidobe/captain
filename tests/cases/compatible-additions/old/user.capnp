@0xbf5147cbbecf40c1;

struct User {
  id @0 :UInt64;
  email @1 :Text;
  status @2 :Status;
}

enum Status {
  active @0;
  disabled @1;
}
