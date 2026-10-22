// The shipped roles (spec/administration.md §23.9.2). Deny by default;
// each role allows its group, and the roles below a storage
// administrator are cumulative. Security is apart, so no one administers
// data and covers their own tracks by default.

@id("base.read")
permit (principal, action in Hyperstor::Action::"read", resource)
when { principal.roles.containsAny(["viewer", "operator", "storage-admin"]) };

@id("base.operate")
permit (principal, action in Hyperstor::Action::"operate", resource)
when { principal.roles.containsAny(["operator", "storage-admin"]) };

@id("base.store")
permit (principal, action in Hyperstor::Action::"store", resource)
when { principal.roles.contains("storage-admin") };

@id("base.secure")
permit (principal, action in Hyperstor::Action::"secure", resource)
when { principal.roles.contains("security-admin") };

// Everyone who logged in manages their own API tokens.
@id("base.own")
permit (principal, action in Hyperstor::Action::"own", resource);

// Step-up: a fresh login, within 10 minutes, for what is hard to undo.
@id("base.step-up")
forbid (principal, action in Hyperstor::Action::"step-up", resource)
when { context.auth.age_secs > 600 };
