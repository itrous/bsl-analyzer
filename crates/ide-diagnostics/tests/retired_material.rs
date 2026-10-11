//! Изъятый тестовый материал в крейт не вернулся.
//!
//! Материал, перенесённый из чужого проекта, однажды уже переехал внутри
//! крейта — из `test_data/` в тело теста, — и удаление файла его не вывело из
//! обращения. Поэтому проверка смотрит на крейт целиком, а не на одно место.
//!
//! Хранится хэш, а не текст: держать текст здесь значило бы держать ровно ту
//! копию, ради устранения которой всё и делается.
//!
//! Provenance: `docs/legal/bsl-clean-room-slice-b2.md`.

use std::path::Path;

/// Отпечаток изъятого материала и его длина в БАЙТАХ.
///
/// Сравнение идёт по байтам, а не по строкам: в исходнике первая строка
/// материала склеена с префиксом объявления (`let code = r#"…`), поэтому
/// построчное окно не совпало бы никогда — такая проверка зелена и при
/// материале на месте. Это проверено: построчный вариант прошёл там, где
/// обязан был упасть.
///
/// Записи замены тестового материала названы носителем и порядковым номером
/// изъятого литерала: `docs/legal/ide-diagnostics-test-material.md`.
const RETIRED: &[(&str, u64, usize)] = &[
    ("фикстура UnknownPreprocessorSymbol", 0xd6fd_8d85_1562_f0d1, 305),
    ("self_assign.rs, вход 1", 0xf47b_5d39_d08f_7fb4, 123),
    ("self_assign.rs, вход 2", 0x4290_4a1a_99ca_a9fc, 71),
    ("self_assign.rs, вход 3", 0x7612_edb6_502e_6a1c, 71),
    ("self_assign.rs, вход 4", 0xac2c_5f37_777d_c7fd, 71),
    ("self_assign.rs, вход 5", 0x57f7_aff7_e5ed_de4c, 427),
    ("function_out_parameter.rs, вход 1", 0x04bf_d63f_55bb_0a41, 223),
    ("function_out_parameter.rs, вход 2", 0x9fe7_729a_f96c_40da, 87),
    ("function_out_parameter.rs, вход 3", 0x0ab4_1d5c_b158_250a, 111),
    ("function_out_parameter.rs, вход 4", 0x6610_29a7_16fa_267e, 131),
    ("function_out_parameter.rs, вход 5", 0xcc4a_f864_0b22_4856, 136),
    ("function_out_parameter.rs, вход 6", 0xce63_7e12_22e9_c1eb, 244),
    ("function_should_have_return.rs, вход 1", 0x2dd9_5659_0e7b_7f17, 96),
    ("function_should_have_return.rs, вход 2", 0xaf13_1459_a58c_7650, 85),
    ("function_should_have_return.rs, вход 3", 0xb596_b311_0701_2004, 110),
    ("function_should_have_return.rs, вход 4", 0x365f_c408_82a7_5bdd, 228),
    ("function_should_have_return.rs, вход 5", 0xc31e_eac7_0432_b45c, 241),
    ("function_should_have_return.rs, вход 6", 0xf95f_8ed3_0932_eef1, 433),
    ("form_data_to_value.rs, вход 1", 0x560a_ab07_98e1_bf9a, 260),
    ("form_data_to_value.rs, вход 2", 0x7202_e85e_eca7_c083, 180),
    ("form_data_to_value.rs, вход 3", 0x910c_8bf4_5d5a_611a, 212),
    ("form_data_to_value.rs, вход 4", 0xbd19_12b5_0877_156e, 230),
    ("form_data_to_value.rs, вход 5", 0xcfc4_9a70_ad3d_ff26, 128),
    ("form_data_to_value.rs, вход 6", 0x9474_ad6f_daf5_d193, 77),
    ("form_data_to_value.rs, вход 7", 0xb32c_8ad3_dcf9_48ed, 162),
    ("form_data_to_value.rs, вход 8", 0xc141_792b_7ea3_ff7a, 173),
    ("form_data_to_value.rs, вход 9", 0x4352_8c39_396e_12a7, 206),
    ("form_data_to_value.rs, вход 10", 0xe4cd_dff0_44eb_2efb, 224),
    ("form_data_to_value.rs, вход 11", 0x26ac_2a42_6d9f_2710, 174),
    ("form_data_to_value.rs, вход 12", 0xe68c_a21a_9bbf_0771, 182),
    ("form_data_to_value.rs, вход 13", 0x5325_253b_077e_1d3b, 80),
    ("form_data_to_value.rs, вход 14", 0xfefb_6e27_74a8_5868, 264),
    ("form_data_to_value.rs, вход 15", 0x04e9_b424_3cd6_4a0d, 119),
    ("deprecated_type_managed_form.rs, вход 1", 0xd461_959b_bb7d_e92b, 202),
    ("deprecated_type_managed_form.rs, вход 2", 0x9580_9550_0431_9421, 105),
    ("deprecated_type_managed_form.rs, вход 3", 0x7730_0efa_33fe_a53e, 129),
    ("deprecated_type_managed_form.rs, вход 4", 0x28d4_cc31_5f8e_036c, 315),
    ("deprecated_type_managed_form.rs, вход 5", 0x611d_ddd9_8ac4_c991, 331),
    ("deprecated_type_managed_form.rs, вход 6", 0x246c_eb39_c2f5_8417, 104),
    ("style_element_constructors.rs, вход 1", 0xb174_59f5_bd6a_d7d2, 1355),
    ("style_element_constructors.rs, вход 2", 0x0940_70bd_3869_6d1b, 109),
    ("style_element_constructors.rs, вход 3", 0xf0df_f4bd_81b1_1b25, 101),
    ("style_element_constructors.rs, вход 4", 0xf24b_ddd3_b3ea_3c12, 174),
    ("nested_constructors_in_structure_declaration.rs, вход 1", 0x4910_1884_8653_6246, 139),
    ("nested_constructors_in_structure_declaration.rs, вход 2", 0xca77_9002_de3a_1c35, 312),
    ("nested_constructors_in_structure_declaration.rs, вход 3", 0xb47e_5244_c826_760a, 244),
    ("nested_constructors_in_structure_declaration.rs, вход 4", 0x8942_acbf_f2fb_5a8b, 126),
    ("nested_constructors_in_structure_declaration.rs, вход 5", 0x4057_178a_2f1c_91e7, 90),
    ("nested_constructors_in_structure_declaration.rs, вход 6", 0x1b6a_ed6a_b7e5_692f, 4063),
    ("ternary_operator_usage.rs, вход 1", 0xd856_a996_f60d_3e63, 817),
    ("ternary_operator_usage.rs, вход 2", 0xf9d7_4df6_2498_0e2d, 126),
    ("ternary_operator_usage.rs, вход 3", 0x7afe_1212_ee42_4a3d, 132),
    ("ternary_operator_usage.rs, вход 4", 0xf9d7_4df6_2498_0e2d, 126),
    ("code_block_before_sub.rs, вход 1", 0x80b4_edcf_a18e_ab0d, 274),
    ("code_block_before_sub.rs, вход 2", 0xa3ba_2b87_d3f9_ac38, 185),
    ("code_block_before_sub.rs, вход 3", 0x4944_1810_12c6_3354, 133),
    ("code_block_before_sub.rs, вход 4", 0xad92_2029_ecb0_8791, 128),
    ("code_block_before_sub.rs, вход 5", 0x5444_8def_4a41_18ab, 119),
    ("code_block_before_sub.rs, вход 6", 0x4be3_1591_e57b_e215, 214),
    ("code_block_before_sub.rs, вход 7", 0x3a85_ab13_5bfb_1e79, 429),
    ("code_block_before_sub.rs, вход 8", 0x9cfc_4596_2779_719d, 71),
    ("code_block_before_sub.rs, вход 9", 0xb56c_a23c_87a2_5b58, 305),
    ("code_block_before_sub.rs, вход 10", 0x6bb4_feb3_aa28_b636, 316),
    ("code_block_before_sub.rs, вход 11", 0x03a1_0f4b_84e6_d096, 353),
    ("code_out_of_region.rs, вход 1", 0xb063_0d09_09d7_2f31, 2567),
    ("code_out_of_region.rs, вход 2", 0x777b_87af_f992_cf1b, 667),
    ("code_out_of_region.rs, вход 3", 0xffe9_0fd7_c222_fffa, 228),
    ("code_out_of_region.rs, вход 4", 0x6fa7_8f68_8c1e_8767, 407),
    ("code_out_of_region.rs, вход 5", 0x5c42_8b86_018c_d85d, 189),
    ("code_out_of_region.rs, вход 6", 0xc657_7f63_4f87_7523, 75),
    ("code_out_of_region.rs, вход 7", 0x8c83_343b_0a9f_f7ed, 188),
    ("code_out_of_region.rs, вход 8", 0x5b40_bd7c_9f51_86a2, 88),
    ("code_out_of_region.rs, вход 9", 0x4ac6_72ed_5f96_7bc7, 139),
    ("commented_code.rs, вход 1", 0xa437_acc6_c70d_a2b0, 90),
    ("commented_code.rs, вход 2", 0xd144_3796_435f_d7af, 152),
    ("commented_code.rs, вход 3", 0xa9e0_60c3_c8c3_17cf, 135),
    ("commented_code.rs, вход 4", 0xae29_bfbc_f023_40fc, 122),
    ("commented_code.rs, вход 5", 0x720f_5406_b165_ead2, 106),
    ("commented_code.rs, вход 6", 0xcc1c_7c63_e108_0a54, 148),
    ("commented_code.rs, вход 7", 0xcc1c_7c63_e108_0a54, 148),
    ("commented_code.rs, вход 8", 0x69d3_b8d1_8135_43a3, 176),
    ("commented_code.rs, вход 9", 0xcf19_5330_0c9c_e6cb, 152),
    ("commented_code.rs, вход 10", 0x5c55_1412_ce17_aa31, 99),
    ("commented_code.rs, вход 11", 0x6d75_a4b5_7b74_e4d1, 91),
    ("commented_code.rs, вход 12", 0x304f_e5c4_4046_3f64, 100),
    ("commented_code.rs, вход 13", 0x3cb3_155c_cf9c_1b97, 179),
    ("commented_code.rs, вход 14", 0x9be2_fb61_8f35_fc2e, 145),
    ("commented_code.rs, вход 15", 0xfaed_0ef5_c6f5_8e4a, 183),
    ("commented_code.rs, вход 16", 0xdeb0_4eff_81c4_ad07, 417),
    ("commented_code.rs, вход 17", 0x6fd0_5d3f_2942_4cd7, 328),
    ("commented_code.rs, вход 18", 0xdaf0_0e53_960c_7d9d, 114),
    ("commented_code.rs, вход 19", 0x52ca_4a4a_41ba_f20f, 218),
    ("commented_code.rs, вход 20", 0x5d91_c4b6_5a1e_0f0f, 70),
    ("commented_code.rs, вход 21", 0x37cb_0908_ee04_d29f, 152),
    ("commented_code.rs, вход 22", 0x825f_c96b_c2e7_66c1, 189),
    ("commented_code.rs, вход 23", 0x3f0d_cd67_2c94_260d, 284),
    ("commented_code.rs, вход 24", 0xe0f2_1d4a_5b00_2fa7, 100),
    ("commented_code.rs, вход 25", 0xde0a_d008_8468_b2cd, 158),
    ("commented_code.rs, вход 26", 0xf99a_8ae7_2b74_aee3, 114),
    ("commented_code.rs, вход 27", 0xf91a_877a_032f_0589, 125),
    ("commented_code.rs, вход 28", 0x4a6b_1091_528d_04b3, 128),
    ("commented_code.rs, вход 29", 0x8a46_9125_6cec_949c, 181),
    ("commented_code.rs, вход 30", 0x5f5b_4378_b620_84c1, 151),
    ("commented_code.rs, вход 31", 0xaeed_dad5_2f84_88a5, 119),
    ("commented_code.rs, вход 32", 0xf6ae_2c7f_0843_d9bd, 151),
    ("commented_code.rs, вход 33", 0x6d86_0174_6df5_092b, 147),
    ("commented_code.rs, вход 34", 0xb2a3_4599_9bc5_9947, 80),
    ("commented_code.rs, вход 35", 0x5665_94f7_147e_47a0, 248),
    ("commented_code.rs, вход 36", 0xd0bf_e589_0654_2fa6, 161),
    ("commented_code.rs, вход 37", 0x6108_1856_d53c_a686, 228),
    ("commented_code.rs, вход 38", 0x409b_ae1b_2507_61fc, 88),
    ("commented_code.rs, вход 39", 0x24aa_4ea9_ecdc_99dc, 109),
    ("commented_code.rs, вход 40", 0x66e1_2003_7756_f9b0, 195),
    ("commented_code.rs, вход 41", 0x35b7_5346_4a09_c2d4, 80),
    ("commented_code.rs, вход 42", 0x619d_4405_af3e_ca47, 134),
    ("compilation_directive_lost.rs, вход 1", 0xd7a7_13a1_c1c8_0240, 272),
    ("compilation_directive_lost.rs, вход 2", 0xdd5c_8acd_e615_737f, 74),
    ("compilation_directive_lost.rs, вход 3", 0x241e_8a5a_e1f8_4190, 75),
    ("compilation_directive_lost.rs, вход 4", 0x017f_0589_8fd1_1a7a, 274),
    ("compilation_directive_lost.rs, вход 5", 0x435b_21bc_ae49_e9d7, 90),
    ("compilation_directive_lost.rs, вход 6", 0x20db_e8ad_d298_b958, 274),
    ("compilation_directive_lost.rs, вход 7", 0x241e_8a5a_e1f8_4190, 75),
    ("excessive_auto_test_check.rs, вход 1", 0x5095_b380_5541_bd39, 224),
    ("excessive_auto_test_check.rs, вход 2", 0x877f_21d8_f2d7_917d, 498),
    ("excessive_auto_test_check.rs, вход 3", 0x1b0a_ac51_01e4_df6d, 234),
    ("excessive_auto_test_check.rs, вход 4", 0xef74_e880_0103_6fe2, 360),
    ("excessive_auto_test_check.rs, вход 5", 0x698d_83bf_9b22_6e72, 126),
    ("excessive_auto_test_check.rs, вход 6", 0x66cd_ddb2_0efd_1dca, 101),
    ("excessive_auto_test_check.rs, вход 7", 0x41e5_4c29_8241_c9ef, 105),
    ("excessive_auto_test_check.rs, вход 8", 0x696f_bab6_59eb_c608, 140),
    ("excessive_auto_test_check.rs, вход 9", 0xf9f8_9447_8fa9_365d, 74),
    ("excessive_auto_test_check.rs, вход 10", 0xae09_87ff_d078_152d, 188),
    ("excessive_auto_test_check.rs, вход 11", 0xdb96_8990_d33a_3c65, 102),
    ("excessive_auto_test_check.rs, вход 12", 0x9d53_1830_4eaa_7670, 174),
    ("excessive_auto_test_check.rs, вход 13", 0x3d9a_22f4_d0bd_ea10, 92),
    ("excessive_auto_test_check.rs, вход 14", 0xf89d_7714_334e_9169, 216),
    ("excessive_auto_test_check.rs, вход 15", 0xaf24_23c1_abc2_3107, 192),
    ("excessive_auto_test_check.rs, вход 16", 0x6278_b6e8_57e1_1ee6, 200),
    ("duplicate_string_literal.rs, вход 1", 0x9cc0_9183_2782_6495, 279),
    ("duplicate_string_literal.rs, вход 2", 0x003e_27b8_8bf1_89ef, 290),
    ("duplicate_string_literal.rs, вход 3", 0x8a22_8846_f8a6_b610, 135),
    ("duplicate_string_literal.rs, вход 4", 0x2380_1a06_3c30_ba91, 105),
    ("duplicate_string_literal.rs, вход 5", 0x0860_e28a_1a40_d989, 108),
    ("duplicate_string_literal.rs, вход 6", 0x9232_7db8_b949_c4c8, 132),
    ("duplicate_string_literal.rs, вход 7", 0x24dd_f594_9f5d_d68c, 258),
    ("duplicate_string_literal.rs, вход 8", 0xf94a_fd02_5101_cda2, 165),
    ("duplicate_string_literal.rs, вход 9", 0x7c4b_54ab_7e0b_5957, 366),
    ("duplicate_string_literal.rs, вход 10", 0x4976_d50b_9f5e_feef, 679),
    ("duplicate_string_literal.rs, вход 11", 0x2ae1_4427_1686_3df0, 300),
    ("duplicate_string_literal.rs, вход 12", 0x0cfb_28f6_1098_5aed, 222),
    ("duplicate_string_literal.rs, вход 13", 0x4c60_d77c_f61e_c5d1, 188),
    ("incorrect_line_break.rs, вход 1", 0x5012_eb9e_71f0_3049, 184),
    ("incorrect_line_break.rs, вход 2", 0xe2c7_d854_df5b_ad60, 144),
    ("incorrect_line_break.rs, вход 3", 0x3847_8dc8_8a53_a51e, 218),
    ("incorrect_line_break.rs, вход 4", 0x6076_d8b2_a52d_8074, 379),
    ("incorrect_line_break.rs, вход 5", 0xa3db_e883_c017_2d71, 234),
    ("incorrect_line_break.rs, вход 6", 0x1a93_4311_2d30_e384, 175),
    ("incorrect_line_break.rs, вход 7", 0xb9f5_2a80_c528_85ba, 84),
    ("incorrect_line_break.rs, вход 8", 0x1a80_0e1d_699c_8727, 158),
    ("incorrect_line_break.rs, вход 9", 0x150e_90bf_e9e0_52c2, 102),
    ("incorrect_line_break.rs, вход 10", 0x6d84_61e4_d147_7b19, 189),
    ("incorrect_line_break.rs, вход 11", 0xea3c_eb0e_767b_b8b3, 117),
    ("incorrect_line_break.rs, вход 12", 0x97a7_59ee_c047_fef2, 112),
    ("line_length.rs, вход 1", 0x303d_7275_3cc8_addb, 5983),
    ("line_length.rs, вход 2", 0x1bd7_881f_2dba_5248, 77),
    ("multilingual_string_using_with_template.rs, вход 1", 0xd89e_a677_1bcd_95c9, 2361),
    ("multilingual_string_using_with_template.rs, вход 2", 0xd89e_a677_1bcd_95c9, 2361),
    ("multilingual_string_using_with_template.rs, вход 3", 0x05e8_e998_4307_f61d, 178),
    ("multilingual_string_using_with_template.rs, вход 4", 0xca3f_19e8_37bb_a7b4, 165),
    ("typo.rs, вход 1", 0x62b7_7470_b3b4_f9da, 724),
    ("cached_public.rs, вход 1", 0xcea1_1a7f_d90f_2bf6, 146),
    ("cached_public.rs, вход 2", 0x6975_fe96_2d44_eb44, 164),
    ("cached_public.rs, вход 4", 0x5cbc_38c6_0d90_b740, 476),
    ("cached_public.rs, вход 5", 0x5cbc_38c6_0d90_b740, 476),
    ("cached_public.rs, вход 6", 0x5cbc_38c6_0d90_b740, 476),
    ("cached_public.rs, вход 7", 0xba2e_5bd8_17bc_75c5, 176),
    ("cached_public.rs, вход 8", 0x125f_920e_202b_19fd, 223),
    ("cached_public.rs, вход 9", 0xa285_6152_4bf6_d731, 309),
    ("command_module_export_methods.rs, вход 1", 0x2c2b_1079_c899_60a6, 266),
    ("command_module_export_methods.rs, вход 2", 0x1583_4598_bc15_5696, 136),
    ("command_module_export_methods.rs, вход 3", 0x7946_382c_3047_d825, 75),
    ("command_module_export_methods.rs, вход 4", 0x9321_d9bb_ba20_82fb, 90),
    ("command_module_export_methods.rs, вход 5", 0x876f_703c_7fff_49f4, 74),
    ("execute_external_code.rs, вход 1", 0xc072_0cc5_fe79_71cd, 188),
    ("execute_external_code.rs, вход 2", 0x928d_1c27_23ca_fcbf, 236),
    ("execute_external_code.rs, вход 3", 0x3b4d_ecbf_f109_77ae, 239),
    ("execute_external_code.rs, вход 4", 0x3235_c047_d5ce_c41d, 141),
    ("execute_external_code.rs, вход 5", 0x7608_6387_d3ef_fb26, 167),
    ("execute_external_code.rs, вход 6", 0xcdfe_b8e9_7748_ad10, 158),
    ("execute_external_code.rs, вход 7", 0x28a8_87f7_5797_6e8a, 158),
    ("execute_external_code.rs, вход 8", 0xfd22_3f14_b67a_e352, 143),
    ("execute_external_code.rs, вход 9", 0x0d1b_2e0e_4187_937e, 144),
    ("execute_external_code.rs, вход 10", 0x363b_efa4_0c36_6461, 137),
    ("execute_external_code.rs, вход 11", 0x30b3_625c_7c3f_93e7, 181),
    ("execute_external_code.rs, вход 12", 0x5ba2_4861_a619_5222, 305),
    ("execute_external_code_in_common_module.rs, вход 1", 0x2c52_0780_5074_6b8d, 443),
    ("execute_external_code_in_common_module.rs, вход 2", 0x2917_237a_32dd_c5fc, 98),
    ("execute_external_code_in_common_module.rs, вход 3", 0x0d1b_2e0e_4187_937e, 144),
    ("execute_external_code_in_common_module.rs, вход 4", 0x363b_efa4_0c36_6461, 137),
    ("global_context_method_collision8312.rs, вход 1", 0x01c8_f6c1_73f4_3c03, 1544),
    ("global_context_method_collision8312.rs, вход 2", 0x2cdf_c834_8e58_c32f, 234),
    ("global_context_method_collision8312.rs, вход 3", 0x2c67_bc39_1a8b_9b55, 66),
    ("global_context_method_collision8312.rs, вход 4", 0xc1c6_275e_1e5c_ade3, 182),
    ("global_context_method_collision8312.rs, вход 5", 0x8d5f_93c9_44cd_aff0, 134),
    ("missing_code_try_catch_ex.rs, вход 1", 0x5a6b_c5c5_c35a_f788, 204),
    ("missing_code_try_catch_ex.rs, вход 2", 0x1450_4822_f0f3_194b, 195),
    ("missing_code_try_catch_ex.rs, вход 3", 0x43df_d1e2_c64e_7c3d, 204),
    ("missing_code_try_catch_ex.rs, вход 4", 0x27d2_031a_0bf0_4973, 208),
    ("missing_code_try_catch_ex.rs, вход 5", 0x6173_4e8c_ab3b_1a17, 243),
    ("missing_code_try_catch_ex.rs, вход 6", 0xec5e_c0e3_1f46_142c, 239),
    ("missing_code_try_catch_ex.rs, вход 7", 0x24cc_d4bb_3947_8214, 1664),
    ("missing_code_try_catch_ex.rs, вход 8", 0x24cc_d4bb_3947_8214, 1664),
    ("missing_code_try_catch_ex.rs, вход 9", 0x6953_9300_f7a0_5bec, 378),
    ("missing_code_try_catch_ex.rs, вход 10", 0x4ebb_f20e_aafb_4f07, 251),
    ("missing_code_try_catch_ex.rs, вход 11", 0x44ae_e399_64db_0dd6, 207),
    ("missing_code_try_catch_ex.rs, вход 12", 0xcab6_a2e3_fb1a_d904, 125),
    ("missing_code_try_catch_ex.rs, вход 13", 0x4c67_e920_29b4_e9a7, 299),
    ("missing_code_try_catch_ex.rs, вход 14", 0xec5e_c0e3_1f46_142c, 239),
    ("missing_code_try_catch_ex.rs, вход 15", 0x27d2_031a_0bf0_4973, 208),
    ("usage_write_log_event.rs, вход 1", 0xd881_182f_2884_a851, 132),
    ("usage_write_log_event.rs, вход 2", 0xce9f_3622_b4ee_28e8, 197),
    ("usage_write_log_event.rs, вход 3", 0x60e1_c349_b38b_9980, 201),
    ("usage_write_log_event.rs, вход 4", 0x71c3_8a7d_e1a2_7c36, 248),
    ("usage_write_log_event.rs, вход 5", 0x1b18_678c_ebc1_35cc, 203),
    ("usage_write_log_event.rs, вход 6", 0x5d5f_d64a_fbf4_874e, 343),
    ("usage_write_log_event.rs, вход 7", 0xf388_e68e_309a_b337, 345),
    ("usage_write_log_event.rs, вход 8", 0x9835_e0f1_6507_1aff, 341),
    ("usage_write_log_event.rs, вход 9", 0x4574_1025_afdd_0878, 443),
    ("usage_write_log_event.rs, вход 10", 0x4203_a9a3_90d9_0736, 364),
    ("usage_write_log_event.rs, вход 11", 0x645f_ae8e_959e_586a, 375),
    ("usage_write_log_event.rs, вход 12", 0x10cd_7553_439f_4f01, 315),
    ("usage_write_log_event.rs, вход 13", 0x2dd0_bf6a_aa11_6e82, 401),
    ("usage_write_log_event.rs, вход 14", 0x9cb1_926a_76cc_8d74, 411),
    ("usage_write_log_event.rs, вход 15", 0xe553_141a_1ddc_3946, 468),
    ("usage_write_log_event.rs, вход 16", 0x683b_7d66_e9ab_7d6e, 443),
    ("usage_write_log_event.rs, вход 17", 0x44a2_ff1f_8595_4240, 512),
    ("usage_write_log_event.rs, вход 18", 0x00ea_52cd_9998_2e8e, 661),
    ("usage_write_log_event.rs, вход 19", 0x5e7c_d3b7_aa0a_16df, 723),
    ("usage_write_log_event.rs, вход 20", 0x1aa4_293a_c41a_ec46, 338),
    ("usage_write_log_event.rs, вход 21", 0x6c0c_beff_bfa6_2e48, 421),
    ("usage_write_log_event.rs, вход 22", 0x7c82_9045_9d89_a359, 829),
    ("usage_write_log_event.rs, вход 23", 0xb596_8f19_eb71_3ffb, 899),
    ("usage_write_log_event.rs, вход 24", 0x28f8_86ec_b871_bc46, 714),
    ("usage_write_log_event.rs, вход 25", 0x0232_5627_4d70_cda9, 132),
    ("usage_write_log_event.rs, вход 26", 0x85c8_ed5d_b0b5_f784, 397),
    ("usage_write_log_event.rs, вход 27", 0x631c_bd7f_b3cd_10b0, 474),
    ("usage_write_log_event.rs, вход 28", 0x4839_777e_7dae_dc82, 543),
    ("usage_write_log_event.rs, вход 29", 0xa2e3_097f_a57f_db12, 723),
    ("usage_write_log_event.rs, вход 30", 0x3d94_1b25_b029_0c21, 754),
    ("usage_write_log_event.rs, вход 31", 0xd212_f8a0_8f82_a188, 499),
    ("usage_write_log_event.rs, вход 32", 0x85c8_ed5d_b0b5_f784, 397),
    ("usage_write_log_event.rs, вход 33", 0x3b12_0b0a_bc2e_e068, 398),
    ("usage_write_log_event.rs, вход 34", 0x77bf_dc07_e147_649b, 860),
    ("usage_write_log_event.rs, вход 35", 0x2cb8_e9cc_3607_0eba, 872),
    ("metadata_object_name_length.rs, вход 1", 0x2688_9f5f_dc03_27ca, 168),
    ("create_query_in_cycle.rs, вход 1", 0x4585_be01_b1e9_0700, 221),
    ("create_query_in_cycle.rs, вход 2", 0x8fb3_c437_7f49_2e9f, 348),
    ("create_query_in_cycle.rs, вход 3", 0xefa6_86de_40d3_1bd3, 129),
    ("create_query_in_cycle.rs, вход 4", 0x3daa_bc16_cd16_716a, 216),
    ("create_query_in_cycle.rs, вход 5", 0x88d7_230b_4b8c_fff6, 204),
    ("fields_from_joins_without_is_null.rs, вход 1", 0xc517_f5b1_89d7_9a8a, 443),
    ("fields_from_joins_without_is_null.rs, вход 2", 0xce5f_346f_7e1a_6ed8, 504),
    ("fields_from_joins_without_is_null.rs, вход 3", 0xe554_d955_aad8_4a60, 547),
    ("fields_from_joins_without_is_null.rs, вход 4", 0x683f_8033_94c5_5341, 492),
    ("fields_from_joins_without_is_null.rs, вход 5", 0x6c57_1c9e_c716_4404, 450),
    ("fields_from_joins_without_is_null.rs, вход 6", 0xcd90_76df_636a_3166, 642),
    ("fields_from_joins_without_is_null.rs, вход 7", 0x757b_d150_3e6f_936f, 520),
    ("fields_from_joins_without_is_null.rs, вход 8", 0x82c1_e9ab_b3a4_2d65, 523),
    ("fields_from_joins_without_is_null.rs, вход 9", 0x8fe6_e39b_4f5c_8efc, 1580),
    ("fields_from_joins_without_is_null.rs, вход 10", 0x05cb_0647_7994_557f, 963),
    ("fields_from_joins_without_is_null.rs, вход 11", 0x8d17_1857_8aa0_4ac9, 600),
    ("fields_from_joins_without_is_null.rs, вход 12", 0xa62b_06b2_5841_6ebb, 605),
    ("fields_from_joins_without_is_null.rs, вход 13", 0xb440_c180_2ded_42a4, 618),
    ("fields_from_joins_without_is_null.rs, вход 14", 0xf94e_2403_a685_5b5a, 718),
    ("fields_from_joins_without_is_null.rs, вход 15", 0x2395_3e9c_d41e_f8c7, 591),
    ("fields_from_joins_without_is_null.rs, вход 16", 0x6307_f97e_51b1_ce87, 625),
    ("fields_from_joins_without_is_null.rs, вход 17", 0xd238_6b3f_08b1_f10d, 592),
    ("fields_from_joins_without_is_null.rs, вход 18", 0xc98b_757d_9f2b_a14c, 634),
    ("fields_from_joins_without_is_null.rs, вход 19", 0x0516_092b_c8c1_6567, 617),
    ("fields_from_joins_without_is_null.rs, вход 20", 0x83a1_6b7b_9256_e369, 622),
    ("fields_from_joins_without_is_null.rs, вход 21", 0xe0b9_32eb_4fa5_9f5d, 611),
    ("fields_from_joins_without_is_null.rs, вход 22", 0xd239_2686_4db5_a74f, 616),
    ("fields_from_joins_without_is_null.rs, вход 23", 0xc649_cda4_e793_10d8, 377),
    ("fields_from_joins_without_is_null.rs, вход 24", 0xee34_75e5_086d_da6a, 624),
    ("fields_from_joins_without_is_null.rs, вход 25", 0x2e58_4546_718b_e984, 510),
    ("logical_or_in_join_query_section.rs, вход 1", 0x6e27_fb9b_7c46_5d00, 4106),
    ("logical_or_in_join_query_section.rs, вход 2", 0xdb77_a589_ba99_f8ac, 220),
    ("logical_or_in_join_query_section.rs, вход 3", 0x5420_daba_367c_c064, 137),
    ("logical_or_in_join_query_section.rs, вход 4", 0xf15a_dd72_65bd_0f34, 181),
    ("logical_or_in_join_query_section.rs, вход 5", 0x4654_d95d_edc7_5a04, 160),
    ("logical_or_in_join_query_section.rs, вход 6", 0x9410_79d7_cde1_2334, 254),
    ("logical_or_in_join_query_section.rs, вход 7", 0xd919_e5d7_15b4_fa48, 270),
    ("logical_or_in_join_query_section.rs, вход 8", 0x44bd_ebc5_bae6_c7f8, 283),
    ("logical_or_in_the_where_section_of_query.rs, вход 1", 0xa050_abab_a413_aa99, 2865),
    ("logical_or_in_the_where_section_of_query.rs, вход 2", 0x27e3_0f00_6615_f233, 138),
    ("logical_or_in_the_where_section_of_query.rs, вход 3", 0xf4a4_e6cd_21c8_ea7c, 170),
    ("logical_or_in_the_where_section_of_query.rs, вход 4", 0x7b22_edf3_2d15_65d6, 130),
    ("logical_or_in_the_where_section_of_query.rs, вход 5", 0xebff_8d1d_6cc1_1453, 127),
    ("logical_or_in_the_where_section_of_query.rs, вход 6", 0x5c31_002d_355c_ae87, 151),
    ("logical_or_in_the_where_section_of_query.rs, вход 7", 0x657c_4582_f099_d3db, 159),
    ("logical_or_in_the_where_section_of_query.rs, вход 8", 0x3dd8_b42e_18a6_eaaa, 157),
    ("logical_or_in_the_where_section_of_query.rs, вход 9", 0xe6b9_75d3_64e0_f903, 104),
    ("logical_or_in_the_where_section_of_query.rs, вход 10", 0xe9ad_0e58_b228_65a1, 340),
    ("logical_or_in_the_where_section_of_query.rs, вход 11", 0x1258_c04f_f7f9_4897, 163),
    ("logical_or_in_the_where_section_of_query.rs, вход 12", 0x4fd8_f125_7c1e_bc28, 650),
    ("logical_or_in_the_where_section_of_query.rs, вход 13", 0xbdf0_95e9_8a29_76ed, 527),
    ("union_all.rs, вход 1", 0xe1e4_b57f_30c0_91d0, 2746),
    ("union_all.rs, вход 2", 0xf19d_fb39_4696_5e04, 86),
    ("union_all.rs, вход 3", 0xff08_8f00_5159_20a9, 155),
    ("union_all.rs, вход 4", 0xf11d_3c87_71d0_1801, 162),
    ("union_all.rs, вход 5", 0x49a3_c0a6_59a5_8448, 138),
    ("union_all.rs, вход 6", 0x5c19_6d36_7b4f_0f41, 142),
    ("union_all.rs, вход 7", 0xc729_6f4c_46bc_85d5, 261),
    ("incorrect_use_like_in_query.rs, вход 1", 0xfa12_c463_d2ef_a043, 2937),
    ("incorrect_use_like_in_query.rs, вход 3", 0xb17e_1764_5ddd_f25f, 175),
    ("incorrect_use_like_in_query.rs, вход 4", 0xeec7_b1ff_a1f2_728e, 182),
    ("incorrect_use_like_in_query.rs, вход 5", 0x9c75_12b2_b1cf_736d, 208),
    ("all_function_path_must_have_return.rs, вход 1", 0x7bb2_7c26_a15a_5f94, 465),
    ("all_function_path_must_have_return.rs, вход 2", 0x6387_fa1a_c91a_e0af, 510),
    ("all_function_path_must_have_return.rs, вход 3", 0x3e1b_2669_16b9_c172, 420),
    ("all_function_path_must_have_return.rs, вход 4", 0xfdb9_8979_a5ed_f89e, 316),
    ("all_function_path_must_have_return.rs, вход 5", 0x7fc6_7dd2_7374_1bfb, 439),
    ("all_function_path_must_have_return.rs, вход 6", 0xa856_8516_ae33_2876, 443),
    ("all_function_path_must_have_return.rs, вход 7", 0xe7b3_1aec_a86d_6646, 135),
    ("all_function_path_must_have_return.rs, вход 8", 0x4e50_e029_50f1_4b54, 244),
    ("all_function_path_must_have_return.rs, вход 9", 0x1b43_4440_8a50_4beb, 1049),
    ("all_function_path_must_have_return.rs, вход 10", 0x81bd_e4e2_d9ee_9ad4, 176),
    ("all_function_path_must_have_return.rs, вход 11", 0x6aed_d4e3_c4cd_aca1, 173),
    ("all_function_path_must_have_return.rs, вход 12", 0x8db9_1b4c_5950_b412, 168),
    ("all_function_path_must_have_return.rs, вход 13", 0x4f87_441d_d2ef_b9ee, 131),
    ("all_function_path_must_have_return.rs, вход 14", 0x9c64_f795_ea80_788a, 290),
    ("all_function_path_must_have_return.rs, вход 15", 0xdb34_0b31_598a_8f8f, 107),
    ("all_function_path_must_have_return.rs, вход 16", 0xb192_500e_7cfb_0c08, 490),
    ("all_function_path_must_have_return.rs, вход 17", 0x1695_5069_9fd7_d45e, 498),
    ("all_function_path_must_have_return.rs, вход 18", 0x86a3_8b95_c4c5_8e3a, 184),
    ("all_function_path_must_have_return.rs, вход 19", 0xcc87_a53c_a575_6113, 141),
    ("cognitive_complexity.rs, вход 1", 0x8951_3f88_39ee_96b9, 127),
    ("cognitive_complexity.rs, вход 2", 0xaeb8_a340_8a1e_a5a9, 256),
    ("cognitive_complexity.rs, вход 3", 0xaf7e_ba64_a155_49df, 441),
    ("cognitive_complexity.rs, вход 4", 0x2ea6_722f_1050_bb27, 384),
    ("cognitive_complexity.rs, вход 5", 0xf360_8e94_a04f_eee6, 190),
    ("cognitive_complexity.rs, вход 6", 0x9abc_35f8_47a4_d764, 1445),
    ("cognitive_complexity.rs, вход 7", 0xcce1_d5a5_b6c5_2001, 192),
    ("cyclomatic_complexity.rs, вход 1", 0x8951_3f88_39ee_96b9, 127),
    ("cyclomatic_complexity.rs, вход 3", 0xdc89_82d9_664e_0e08, 2302),
    ("cyclomatic_complexity.rs, вход 4", 0xdc89_82d9_664e_0e08, 2302),
    ("method_size.rs, вход 1", 0x6ee3_7f18_7874_2fb7, 88),
    ("method_size.rs, вход 2", 0x2381_defc_88a6_afbe, 82),
    ("method_size.rs, вход 4", 0x5c92_231f_1cce_e4aa, 101),
    ("if_condition_complexity.rs, вход 1", 0xf8fc_952b_984c_e082, 147),
    ("if_condition_complexity.rs, вход 2", 0x5e81_bd36_b96d_ef5f, 157),
    ("if_condition_complexity.rs, вход 3", 0xb79f_e9dd_ebbe_666a, 163),
    ("if_condition_complexity.rs, вход 4", 0xaac9_69c7_e9fe_657e, 230),
    ("if_condition_complexity.rs, вход 5", 0x2d69_953d_d7cd_4a01, 94),
    ("if_condition_complexity.rs, вход 6", 0xe4fd_5901_19c9_2272, 986),
    ("if_condition_complexity.rs, вход 7", 0xdb8d_4723_6d2d_f123, 718),
    ("if_condition_complexity.rs, вход 8", 0xf8fc_952b_984c_e082, 147),
    ("if_condition_complexity.rs, вход 9", 0xe527_f265_0e74_89f9, 925),
    ("if_else_duplicated_condition.rs, вход 1", 0x685b_3b20_a3ec_003b, 242),
    ("if_else_duplicated_condition.rs, вход 2", 0x54e6_bbb3_4d3c_a23d, 242),
    ("if_else_duplicated_condition.rs, вход 3", 0xc3ba_37a5_7fd1_1d6d, 188),
    ("if_else_duplicated_condition.rs, вход 4", 0x0411_196a_c04b_662d, 194),
    ("if_else_duplicated_condition.rs, вход 5", 0xe046_7c59_2c07_82be, 230),
    ("if_else_duplicated_condition.rs, вход 6", 0x293b_99c2_7df4_decf, 230),
    ("if_else_duplicated_condition.rs, вход 7", 0x4438_2e34_a38a_c949, 320),
    ("if_else_duplicated_condition.rs, вход 8", 0x4b14_48f8_45b5_1750, 397),
    ("if_else_duplicated_condition.rs, вход 9", 0x1623_3be3_8c0d_1a9e, 513),
    ("if_else_duplicated_condition.rs, вход 10", 0xbaf4_6bbe_8fa5_c42e, 271),
    ("if_else_duplicated_condition.rs, вход 11", 0xc906_b2a1_a4be_76d3, 349),
    ("rewrite_method_parameter.rs, вход 1", 0x22c6_b4d1_a32d_f94c, 117),
    ("rewrite_method_parameter.rs, вход 2", 0x8f50_b1f8_dcf6_7846, 164),
    ("rewrite_method_parameter.rs, вход 3", 0x6471_c6df_411d_da02, 148),
    ("rewrite_method_parameter.rs, вход 4", 0x3a69_575b_62a5_9244, 180),
    ("rewrite_method_parameter.rs, вход 5", 0x1ac2_a3df_b7b5_7377, 320),
    ("rewrite_method_parameter.rs, вход 6", 0xbde0_7671_6b14_2587, 361),
    ("rewrite_method_parameter.rs, вход 7", 0x166e_a315_b91b_b1bf, 306),
    ("rewrite_method_parameter.rs, вход 8", 0x8714_0b48_5a7f_d005, 314),
    ("rewrite_method_parameter.rs, вход 9", 0x8f81_07c2_e0a8_f56d, 377),
    ("rewrite_method_parameter.rs, вход 10", 0x2f63_2373_45af_369b, 292),
    ("rewrite_method_parameter.rs, вход 11", 0x927c_40af_59ea_4edd, 257),
    ("rewrite_method_parameter.rs, вход 12", 0x5c37_3916_69a6_228b, 2591),
    ("deleting_collection_item.rs, вход 1", 0x564f_ac47_6292_7307, 207),
    ("deleting_collection_item.rs, вход 2", 0x4f93_8241_af1f_976a, 209),
    ("deleting_collection_item.rs, вход 3", 0x3607_96fb_e783_7f7d, 188),
    ("deleting_collection_item.rs, вход 4", 0x4003_f4c0_4d1d_5778, 86),
    ("deleting_collection_item.rs, вход 5", 0x86c9_0db2_fe42_2c26, 120),
    ("deleting_collection_item.rs, вход 6", 0x7dc3_1807_f186_042f, 289),
    ("deleting_collection_item.rs, вход 7", 0xf334_6f09_7532_f663, 260),
    ("deleting_collection_item.rs, вход 8", 0x1dda_59ea_d0de_a489, 329),
    ("deleting_collection_item.rs, вход 9", 0x4003_f4c0_4d1d_5778, 86),
    ("deleting_collection_item.rs, вход 10", 0xda70_2a03_0966_cd09, 90),
    ("deleting_collection_item.rs, вход 11", 0x564f_ac47_6292_7307, 207),
    ("deleting_collection_item.rs, вход 12", 0x93a8_3ca4_3150_1bed, 279),
    ("deleting_collection_item.rs, вход 13", 0xe066_8562_a9e9_a8d4, 88),
    ("deleting_collection_item.rs, вход 14", 0x9128_3782_61f1_13c4, 116),
    ("deleting_collection_item.rs, вход 15", 0xc99a_e91f_2911_f61e, 280),
    ("deleting_collection_item.rs, вход 16", 0x314e_ad72_52f3_a057, 356),
    ("deleting_collection_item.rs, вход 17", 0x39aa_ef53_c622_99bf, 754),
    ("deleting_collection_item.rs, вход 18", 0x9591_4edd_41f2_225f, 348),
    ("deleting_collection_item.rs, вход 19", 0xf1b9_12b0_f724_d6e9, 419),
    ("assign_alias_fields_in_query.rs, вход 1", 0x88b2_4ed8_1fa5_13d9, 602),
    ("assign_alias_fields_in_query.rs, вход 2", 0x4baf_660a_b7ba_bc2c, 236),
    ("assign_alias_fields_in_query.rs, вход 3", 0x506f_7593_6a3e_24ac, 240),
    ("assign_alias_fields_in_query.rs, вход 4", 0x1d9b_3760_9731_7844, 416),
    ("assign_alias_fields_in_query.rs, вход 5", 0xb948_14d5_ecf7_0ff0, 160),
    ("assign_alias_fields_in_query.rs, вход 6", 0xb948_14d5_ecf7_0ff0, 160),
    ("assign_alias_fields_in_query.rs, вход 7", 0xd443_7619_cff7_72d6, 128),
    ("assign_alias_fields_in_query.rs, вход 8", 0x777d_837c_dbac_5d9c, 153),
    ("assign_alias_fields_in_query.rs, вход 9", 0x8e75_1e65_3a01_6b36, 484),
    ("assign_alias_fields_in_query.rs, вход 10", 0x5b58_738f_e5e4_6ae4, 972),
    ("assign_alias_fields_in_query.rs, вход 11", 0x52a8_a966_817d_c101, 314),
    ("assign_alias_fields_in_query.rs, вход 12", 0x1765_b3c1_501c_c104, 900),
    ("assign_alias_fields_in_query.rs, вход 13", 0x838c_3b5d_7bba_4e8e, 529),
    ("assign_alias_fields_in_query.rs, вход 14", 0x1076_2dae_c2db_37b9, 285),
    ("using_like_in_query.rs, вход 1", 0x14e1_1730_c906_6d34, 2860),
    ("using_like_in_query.rs, вход 2", 0xedfb_d65f_02cf_8d26, 93),
    ("using_like_in_query.rs, вход 3", 0xe3da_3f97_83ba_3c4c, 174),
    ("using_like_in_query.rs, вход 4", 0xab9f_435c_7ff5_333e, 179),
    ("using_like_in_query.rs, вход 5", 0x8f5f_13b2_be30_3a90, 157),
    ("using_like_in_query.rs, вход 6", 0x65e7_d52c_a57e_5ac0, 192),
    ("using_like_in_query.rs, вход 7", 0xfa5d_f0cf_c611_c231, 303),
    ("full_outer_join_query.rs, вход 1", 0xb559_ded1_26af_f008, 1080),
    ("full_outer_join_query.rs, вход 2", 0x92fd_af49_8f70_9ed9, 847),
    ("full_outer_join_query.rs, вход 3", 0xd508_cb1c_9031_e05b, 93),
    ("full_outer_join_query.rs, вход 4", 0x5c43_a978_90d4_0432, 167),
    ("full_outer_join_query.rs, вход 5", 0x3d2f_7385_4726_c4cb, 378),
    ("full_outer_join_query.rs, вход 6", 0xafda_ef21_1f1f_d652, 148),
    ("full_outer_join_query.rs, вход 7", 0xf9ec_ef11_523a_3c27, 160),
    ("full_outer_join_query.rs, вход 8", 0x2c19_2c55_a79d_bad4, 257),
    ("full_outer_join_query.rs, вход 9", 0xf1e7_88cd_3e32_4bef, 269),
    ("full_outer_join_query.rs, вход 10", 0xaeb0_d211_2b76_a227, 833),
    ("full_outer_join_query.rs, вход 11", 0xc8d8_58e3_e917_dc3a, 586),
    ("join_with_virtual_table.rs, вход 1", 0x4304_052d_4ccd_9253, 374),
    ("join_with_virtual_table.rs, вход 2", 0x5b87_801a_7278_0753, 439),
    ("join_with_virtual_table.rs, вход 3", 0x4a11_3ae3_912a_7cdc, 445),
    ("join_with_virtual_table.rs, вход 4", 0xe6d2_9fc8_2b00_c904, 484),
    ("join_with_virtual_table.rs, вход 5", 0xf3c5_77c9_eb8f_10e1, 440),
    ("join_with_virtual_table.rs, вход 6", 0x5f60_7240_60bf_6f8e, 227),
    ("join_with_virtual_table.rs, вход 8", 0x36aa_3863_fdb5_17b0, 209),
    ("join_with_virtual_table.rs, вход 9", 0x3b0d_1056_d27f_21dd, 242),
    ("join_with_virtual_table.rs, вход 10", 0xcdec_a283_2691_4fa0, 385),
    ("multiline_string_in_query.rs, вход 1", 0x29d0_4933_8f0b_9fe3, 1216),
    ("multiline_string_in_query.rs, вход 2", 0x0edf_12af_cd5d_4944, 629),
    ("multiline_string_in_query.rs, вход 3", 0xb7e1_c368_8d83_9f7e, 256),
    ("select_top_without_order_by.rs, вход 1", 0xdd27_cedf_ac59_f89f, 627),
    ("select_top_without_order_by.rs, вход 2", 0x8ae7_c170_ad1c_f267, 519),
    ("select_top_without_order_by.rs, вход 3", 0x1a99_df03_1c4e_cd37, 518),
    ("select_top_without_order_by.rs, вход 4", 0x875e_51ab_338e_a6bc, 546),
    ("select_top_without_order_by.rs, вход 5", 0x929a_a371_1289_a78c, 545),
    ("select_top_without_order_by.rs, вход 6", 0x3e39_2b15_bb90_8470, 606),
    ("select_top_without_order_by.rs, вход 7", 0x73a2_9264_f71c_cf06, 1142),
    ("select_top_without_order_by.rs, вход 8", 0xde58_b667_594d_475d, 1076),
    ("select_top_without_order_by.rs, вход 9", 0xa564_6b12_ffb6_8fbb, 838),
    ("select_top_without_order_by.rs, вход 10", 0xd736_056b_36bb_706a, 226),
    ("select_top_without_order_by.rs, вход 11", 0x66bf_7282_7fef_926c, 154),
    ("select_top_without_order_by.rs, вход 12", 0xa992_66c0_dbcf_19c4, 207),
    ("select_top_without_order_by.rs, вход 13", 0x6f52_8de8_3f95_4743, 177),
    ("select_top_without_order_by.rs, вход 14", 0x4f5a_dfaa_28e0_353c, 153),
    ("select_top_without_order_by.rs, вход 15", 0xc925_8385_218b_cd34, 206),
    ("select_top_without_order_by.rs, вход 16", 0x20d4_30e0_146b_7883, 84),
    ("virtual_table_call_without_parameters.rs, вход 1", 0xfae7_d8c1_a20f_761f, 2923),
    ("virtual_table_call_without_parameters.rs, вход 2", 0x2ea0_51bc_a832_ffcc, 199),
    ("virtual_table_call_without_parameters.rs, вход 3", 0x39bb_e5a7_7bc0_6a30, 188),
    ("virtual_table_call_without_parameters.rs, вход 4", 0x0626_da4b_83c6_e018, 201),
    ("virtual_table_call_without_parameters.rs, вход 5", 0x62a7_4149_c6c7_ebda, 173),
    ("virtual_table_call_without_parameters.rs, вход 6", 0x07f4_aec6_47c5_572a, 169),
    ("virtual_table_call_without_parameters.rs, вход 7", 0xe626_d7b1_ab58_d07b, 184),
    ("virtual_table_call_without_parameters.rs, вход 8", 0x14ab_6b93_b91c_4f76, 171),
    ("query_nested_fields_by_dot.rs, вход 1", 0xc502_2983_25e6_b6b7, 12132),
    ("join_with_sub_query.rs, вход 1", 0xe619_b638_b35e_b72b, 510),
    ("query_parse_error.rs, вход 1", 0xaf9f_562f_b985_ba03, 1688),
    ("query_parse_error.rs, вход 2", 0x71f7_081d_7351_de2a, 771),
];

const BASE: u64 = 257;

/// Отпечатки из `fingerprints`, совпавшие хотя бы с одним окном длины `span`.
///
/// Полиномиальный хэш окна считается сдвигом за один проход; все отпечатки
/// одной длины проверяются в этом же проходе.
fn matching_windows(haystack: &[u8], span: usize, fingerprints: &[u64]) -> Vec<u64> {
    let mut found = Vec::new();
    if haystack.len() < span || span == 0 {
        return found;
    }

    let mut top = 1u64;
    for _ in 1..span {
        top = top.wrapping_mul(BASE);
    }

    let mut hash = 0u64;
    for byte in &haystack[..span] {
        hash = hash.wrapping_mul(BASE).wrapping_add(u64::from(*byte));
    }
    let mut note = |hash: u64| {
        if fingerprints.contains(&hash) && !found.contains(&hash) {
            found.push(hash);
        }
    };
    note(hash);

    for index in span..haystack.len() {
        hash = hash
            .wrapping_sub(u64::from(haystack[index - span]).wrapping_mul(top))
            .wrapping_mul(BASE)
            .wrapping_add(u64::from(haystack[index]));
        note(hash);
    }

    found
}

fn window_matches(haystack: &[u8], span: usize, fingerprint: u64) -> bool {
    !matching_windows(haystack, span, &[fingerprint]).is_empty()
}

#[test]
fn byte_window_detects_embedded_material_and_rejects_changed_bytes() {
    let material = b"synthetic\nwindow\n";
    let fingerprint = material
        .iter()
        .fold(0u64, |hash, byte| hash.wrapping_mul(BASE).wrapping_add(u64::from(*byte)));
    for (prefix, suffix) in
        [(b"".as_slice(), b"".as_slice()), (b"let code = ", b";"), (b"", b"tail")]
    {
        let text = [prefix, material, suffix].concat();
        assert!(window_matches(&text, material.len(), fingerprint));
    }
    assert!(!window_matches(&material[..material.len() - 1], material.len(), fingerprint));
    let mut changed = material.to_vec();
    changed[0] = b'S';
    assert!(!window_matches(&changed, material.len(), fingerprint));
    assert!(!window_matches(b"", material.len(), fingerprint));

    let other = b"different\nwindow\n";
    let other_fingerprint = other
        .iter()
        .fold(0u64, |hash, byte| hash.wrapping_mul(BASE).wrapping_add(u64::from(*byte)));
    assert_eq!(other.len(), material.len());
    let both = [b"head ".as_slice(), material, b" mid ", other, b" tail"].concat();
    let mut found = matching_windows(&both, material.len(), &[other_fingerprint, fingerprint]);
    found.sort_unstable();
    let mut expected = vec![fingerprint, other_fingerprint];
    expected.sort_unstable();
    assert_eq!(found, expected, "все отпечатки одной длины находятся за один проход");
}

/// Файлы крейта, которые Git отслеживает, без разбора расширений.
///
/// Отбор по расширению здесь был бы дырой, а не оптимизацией: материал уже
/// однажды сменил форму — из `.bsl`-фикстуры он переехал в `.rs`, — и ничто не
/// мешает ему вернуться третьей. В крейте лежат ещё и `xml`, и `json`, и
/// словари, а текст, спрятанный в комментарии разметки, остаётся тем же
/// текстом. Нечитаемое как UTF-8 отсеется само при чтении.
///
/// Зато отбор по отслеживаемости обязателен: в поддереве крейта живут
/// игнорируемые каталоги состояния агентских сессий (`.omc/`), и процитированный
/// там старый текст красил бы гейт, ничего не вернув ни в репозиторий, ни в
/// поставку. Именно отслеживаемый файл — то, что попадает в историю и наружу.
fn tracked_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut command = std::process::Command::new("git");
    // Унаследованные GIT_* бьют `current_dir`: под pre-commit хуком GIT_DIR указывает на
    // корень репозитория, и листинг возвращает пути всего воркспейса вместо путей крейта.
    // Приклеенные к `root`, они не существуют, нечитаемое отсеивается, и гейт зеленеет,
    // не прочитав ни одного файла.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(&key);
        }
    }
    let listing = command.arg("ls-files").arg("-z").current_dir(root).output();

    if let Ok(listing) = listing {
        if listing.status.success() {
            let paths: Vec<_> = listing
                .stdout
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
                .map(|entry| root.join(String::from_utf8_lossy(entry).as_ref()))
                .collect();
            if !paths.is_empty() {
                return paths;
            }
        }
    }

    // Git недоступен или крейт распакован вне своего репозитория: посторонних
    // файлов в такой поставке нет по построению, поэтому берём поддерево целиком.
    let mut paths = Vec::new();
    every_file(root, &mut paths);
    paths
}

fn every_file(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            every_file(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn synthetic_repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("create synthetic repository");
    std::fs::create_dir_all(repo.path().join("crates/diagnostics/nested")).unwrap();
    for (name, text) in [
        (".gitignore", "ignored.xml\nuntracked.json\n"),
        ("crates/diagnostics/sample.rs", "// independent control\n"),
        ("crates/diagnostics/nested/input with spaces.xml", "<probe>synthetic</probe>\n"),
        ("crates/diagnostics/sample.bsl", "// synthetic control\n"),
        ("crates/diagnostics/nested/data.json", "{\"control\":\"synthetic\"}\n"),
        ("crates/diagnostics/nested/material.fixture", "independent format control\n"),
        ("crates/diagnostics/ignored.xml", "<tracked/>\n"),
        ("outside.json", "{}\n"),
    ] {
        std::fs::write(repo.path().join(name), text).unwrap();
    }
    for args in [vec!["init", "--quiet"], vec!["add", "--force", "."]] {
        let mut command = std::process::Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        let output = command.args(args).current_dir(repo.path()).output().expect("run git");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    std::fs::write(repo.path().join("crates/diagnostics/untracked.json"), "{}\n").unwrap();
    repo
}

fn assert_synthetic_tracked_files(root: &Path) {
    let mut files = tracked_files(root);
    files.sort();
    let mut expected = [
        "ignored.xml",
        "nested/data.json",
        "nested/input with spaces.xml",
        "nested/material.fixture",
        "sample.bsl",
        "sample.rs",
    ]
    .map(|name| root.join(name));
    expected.sort();
    assert_eq!(files, expected, "all tracked formats, only inside the crate");
    assert!(files.iter().all(|path| path.is_file()));
}

#[test]
fn tracked_files_include_non_rust_formats_but_not_untracked_files() {
    let repo = synthetic_repository();
    assert_synthetic_tracked_files(&repo.path().join("crates/diagnostics"));
}

#[test]
fn tracked_files_ignore_inherited_git_environment() {
    const ROOT_ENV: &str = "RETIRED_MATERIAL_SYNTHETIC_ROOT";
    if let Some(root) = std::env::var_os(ROOT_ENV) {
        assert_synthetic_tracked_files(Path::new(&root));
        return;
    }

    let repo = synthetic_repository();
    let foreign = synthetic_repository();
    // A child keeps the process environment of other tests untouched.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tracked_files_ignore_inherited_git_environment", "--nocapture"])
        .env(ROOT_ENV, repo.path().join("crates/diagnostics"))
        .env("GIT_DIR", foreign.path().join(".git"))
        .env("GIT_WORK_TREE", foreign.path())
        .env("GIT_INDEX_FILE", foreign.path().join(".git/index"))
        .output()
        .expect("run isolated Git-environment control");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
}

#[test]
fn byte_windows_find_last_utf8_window_and_deduplicate_repetitions() {
    let material = "синтетический контроль\n".as_bytes();
    let fingerprint = material
        .iter()
        .fold(0u64, |hash, byte| hash.wrapping_mul(BASE).wrapping_add(u64::from(*byte)));
    let last_window = [b"prefix".as_slice(), material].concat();
    assert!(window_matches(&last_window, material.len(), fingerprint));
    let text = [b"prefix".as_slice(), material, b"middle", material].concat();
    assert_eq!(
        matching_windows(&text, material.len(), &[fingerprint, fingerprint]),
        vec![fingerprint]
    );
    assert!(window_matches(&text, material.len(), fingerprint));
    assert!(matching_windows(&text, 0, &[fingerprint]).is_empty());
    assert!(matching_windows(&text, material.len(), &[]).is_empty());
    let mut changed = material.to_vec();
    *changed.last_mut().unwrap() = b'!';
    assert!(!window_matches(&changed, material.len(), fingerprint));
}

/// Ни один файл крейта не содержит изъятого материала.
///
/// Граница проверки названа прямо: она ловит дословное возвращение, а не
/// пересказ — сдвиг одного пробела отпечаток не совпадёт. Пересказ остаётся
/// предметом суждения при ревью, и тест его не заменяет. Гейт закрывает то,
/// что произошло на самом деле: перенос знак в знак при смене формы тестов.
#[test]
fn retired_material_did_not_come_back() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = tracked_files(root);

    assert!(
        !files.is_empty(),
        "обход не нашёл ни одного файла — проверка была бы зелена вхолостую"
    );

    let mut by_span: std::collections::BTreeMap<usize, Vec<u64>> = Default::default();
    for (_, fingerprint, span) in RETIRED {
        by_span.entry(*span).or_default().push(*fingerprint);
    }

    // Every file is scanned once per distinct span. Each (file, span) pair is a unit of
    // work taken by the next free thread, so one large file (the dictionaries) does not
    // serialize the gate as the list of retired fingerprints grows.
    let texts: Vec<(&std::path::PathBuf, String)> = files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok().map(|text| (path, text)))
        .collect();
    let spans: Vec<(&usize, &Vec<u64>)> = by_span.iter().collect();
    let units = texts.len() * spans.len();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (texts, spans, next) = (&texts, &spans, &next);
    let mut breaches: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let mut found = Vec::new();
                    loop {
                        let unit = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if unit >= units {
                            break;
                        }
                        let (path, text) = &texts[unit / spans.len()];
                        let (span, fingerprints) = spans[unit % spans.len()];
                        for hit in matching_windows(text.as_bytes(), *span, fingerprints) {
                            for (name, _, _) in
                                RETIRED.iter().filter(|(_, fp, len)| *fp == hit && len == span)
                            {
                                found.push(format!("{name} — {}", path.display()));
                            }
                        }
                    }
                    found
                })
            })
            .collect();
        workers.into_iter().flat_map(|worker| worker.join().expect("scan thread")).collect()
    });
    breaches.sort();

    assert!(breaches.is_empty(), "изъятый материал на месте:\n  {}", breaches.join("\n  "));
}
