Процедура ТолькоИсходник() Экспорт
 P = Справочники.BA252Partners.ПустаяСсылка();
 O = Справочники.BA252Other.ПустаяСсылка();
 BoundDot = P.Caption;
 BoundKey = P["Caption"];
 OtherDot = O.Caption;
 OtherKey = O["Caption"];
 S = Новый Структура("Caption", 1);
 StructureKey = S.Caption;
 CaptionText = "Caption";
 ObjectText = "BA252Partners";
 // Caption BA252Partners — ordinary comment, not a metadata use.
 Q = Новый Запрос("SELECT P.Caption AS StableCaption, O.Caption AS OtherCaption FROM Catalog.BA252Partners AS P INNER JOIN Catalog.BA252Other AS O ON FALSE");
КонецПроцедуры
