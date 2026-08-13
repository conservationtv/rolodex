import { eslintCompatPlugin } from "@oxlint/plugins";

import { noInlineObjectTypeRule } from "./rules/no-inline-object-type.ts";
import { noPrivateClassFieldsRule } from "./rules/no-private-class-fields.ts";
import { requirePascalCaseSchemaNameRule } from "./rules/require-pascal-case-schema-name.ts";
import { requireZodOutputTypeRule } from "./rules/require-zod-output-type.ts";

/** Project-specific Oxlint rules for ConservationTV client style. */
const housePlugin = eslintCompatPlugin({
  meta: { name: "house" },
  rules: {
    "no-inline-object-type": noInlineObjectTypeRule,
    "no-private-class-fields": noPrivateClassFieldsRule,
    "require-pascal-case-schema-name": requirePascalCaseSchemaNameRule,
    "require-zod-output-type": requireZodOutputTypeRule,
  },
});

export default housePlugin;
