macro_rules! bind { ($($name:ident),* $(,)?) => { &[ $(stringify!($name)),* ] }; }
