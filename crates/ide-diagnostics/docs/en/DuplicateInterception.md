# Duplicate interception (DuplicateInterception)

Warns when one physical extension module repeats the same interception kind for
the same target method. Both interceptor names are reported.

```bsl
&After("BeforeWrite")
Procedure FillData()
EndProcedure

&After("BeforeWrite")
Procedure RecordHistory()
EndProcedure
```

Review both actions and combine them into one interceptor if appropriate. There
is no automatic fix: the analyzer does not choose execution order or remove code.

The key is the target method, annotation kind and source module. Names are
case-insensitive, and Russian/English annotation spellings represent the same
kind. Different targets, kinds and modules are not one duplicate. Known mutually
exclusive compilation conditions are excluded; unknown conditions are not guessed.

The warning does not claim that loading or applying an extension must fail.
Interceptor behavior requires a separate check on the target platform.
