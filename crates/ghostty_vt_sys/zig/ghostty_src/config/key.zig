const std = @import("std");
const Config = @import("Config.zig");

/// Key is an enum of all the available configuration keys. This is used
/// when paired with diff to determine what fields have changed in a config,
/// amongst other things.
pub const Key = key: {
    const field_infos = std.meta.fields(Config);
    const Tag = std.math.IntFittingRange(0, field_infos.len - 1);
    var names: [field_infos.len][:0]const u8 = undefined;
    var values: [field_infos.len]Tag = undefined;
    var i: usize = 0;
    for (field_infos) |field| {
        // Ignore fields starting with "_" since they're internal and
        // not copied ever.
        if (field.name[0] == '_') continue;

        names[i] = field.name;
        values[i] = @intCast(i);
        i += 1;
    }

    break :key @Enum(
        Tag,
        .exhaustive,
        names[0..i],
        values[0..i],
    );
};

/// Returns the value type for a key
pub fn Value(comptime key: Key) type {
    const field = comptime field: {
        @setEvalBranchQuota(100_000);

        const fields = std.meta.fields(Config);
        for (fields) |field| {
            if (@field(Key, field.name) == key) {
                break :field field;
            }
        }

        unreachable;
    };

    return field.type;
}

test "Value" {
    const testing = std.testing;

    try testing.expectEqual(Config.RepeatableString, Value(.@"font-family"));
    try testing.expectEqual(?bool, Value(.@"cursor-style-blink"));
}
