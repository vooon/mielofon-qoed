'use strict';

/* Thin seam over the uclient module.
 *
 * The uclient C binding registers its constructor under the reserved name
 * `new`, so it can only be called as `uc.new(...)` (valid in a .uc module but
 * awkward, and impossible to mirror inside a mocked .uc module). Routing the
 * call through here lets unit tests mock `transport` instead of `uclient`.
 */

import * as uc from 'uclient';

export function new_client(url, auth, cb)
{
	return uc.new(url, auth, cb);
};