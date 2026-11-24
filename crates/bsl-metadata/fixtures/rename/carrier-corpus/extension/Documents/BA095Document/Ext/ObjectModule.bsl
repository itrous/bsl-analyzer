&После("BA095Same")
Процедура BA095First(След)
    След.Добавить("after/first");
КонецПроцедуры

&Перед("BA095Same")
Процедура BA095Before(След)
    След.Добавить("before/same");
КонецПроцедуры

&После("BA095Other")
Процедура BA095OtherAfter(След)
    След.Добавить("after/other");
КонецПроцедуры

&После("BA095Split")
Процедура BA095ObjectSplitAfter(След)
    След.Добавить("after/object-split");
КонецПроцедуры
