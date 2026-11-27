const eightBits = ($) =>
  seq("'", ...Array.from({ length: 8 }, () => $._bit), '"');

module.exports = grammar({
  name: 'star',

  extras: ($) => [/[ \t\r\n]+/],

  rules: {
    source_file: ($) => repeat($.statement),
    statement: ($) =>
      choice(
        $.basic_instruction,
        $.extended_instruction,
        $.data_literal,
      ),
    basic_instruction: ($) => seq(repeat1('*'), "'"),
    extended_instruction: ($) => eightBits($),
    data_literal: ($) => seq('"', repeat1($._bit), '"'),
    _bit: ($) => choice('*', "'"),
  },
});
