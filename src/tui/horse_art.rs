//! jockey's mark: the user's own ASCII galloping horse and jockey (70-column
//! render, 7-level terminal ramp), scaled down by averaging its density.
//! Hand-maintained: edit directly.

/// 30 columns, for windows with room.
pub const MARK: [&str; 15] = [
    "                   .=-",
    "              -=-==o#o.",
    "              --ooo=oo  -==",
    "               -=o=.-#oo###-",
    "          ...--oo=oo###@###o.",
    "        .oo-=########@@@o##o-",
    "    .--=o#==#=#######@@#  =o.",
    "  -=o#oo@##oo##@#@##@@@#",
    "  .#@=  -##@@@####@@@@@@.",
    "   --    .##@@@o#@##@@@#-",
    "           o##-  ---=#=o#-.",
    "           .@#      .o   =o.",
    "           -@-=-    .o    .o.",
    "            o. --   ==     .=.",
    "            -.",
];

/// 22 columns, for short windows.
pub const MARK_SMALL: [&str; 11] = [
    "           ...-=.",
    "          -==o##.  .",
    "           -o=.ooo#o.",
    "      .--=o#o###@###=",
    "   .--#o=oo####@@-.o=",
    " .=o#o@#o##@###@@-",
    "  =#. =##@###@@@@=",
    "       .#@o-=oo###-",
    "        =#-    =..-=",
    "        -=.=.  o.   =.",
    "        .-     .     .",
];
